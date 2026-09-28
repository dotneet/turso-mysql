# MySQL compatibility matrix

For what is *not* done yet, read [TODO.md](TODO.md) instead: this file explains
what works and where it differs from MySQL, at length, while that one is the
checklist you read to pick up the next piece of work.

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

`EXPLAIN <table>` prints what `DESCRIBE <table>` prints, which is what MySQL
makes it — measured on 8.4.11, the two answer the same six columns and the same
rows. `EXPLAIN <statement>` is a different thing entirely, the optimizer's own
plan over twelve columns, and it stays refused: answering it would mean writing
down a join order, a key choice and a row estimate this server does not make.
Anything after `EXPLAIN` that is not one lone table name is left to the ordinary
path, so `EXPLAIN FORMAT = JSON ...` and `EXPLAIN ANALYZE ...` are refused with
it.

A pattern names the columns to report. `SHOW COLUMNS FROM t LIKE 'n%'` answers
the columns whose names it matches, and `DESCRIBE t <name>` reads a name after
the table the same way — measured on 8.4.11, quoted or not, the two answer the
same rows, and a pattern nothing matches answers no rows rather than an error.
The pattern is read only for `DESCRIBE` and `DESC`: after `EXPLAIN` a second
word is as likely to be the statement whose plan was asked for, and no shape
tells `EXPLAIN t 1` from `EXPLAIN SELECT 1`. `DESCRIBE TABLE t` is refused —
measured, MySQL reads the `TABLE` there as a keyword and names `t` as the
table, where a plain reading would name `TABLE`, and answering about the wrong
table is worse than refusing.

`SHOW FULL COLUMNS` answers three columns more. Measured on 8.4.11: `Collation`
sits third and `Privileges` and `Comment` follow `Extra`. The collation is the
text one for a `VARCHAR`, `CHAR` or `TEXT` and NULL for every other type, a
`VARBINARY` and a `BLOB` included. The comment is the text the column was
declared with, empty where it was declared with none.

`Privileges` reports the connected user's effective database or table grants.
A database-wide query grant reports `select,insert,update,references`; narrower
table grants report the actions actually granted. Column-specific grants are
not supported.

`SHOW COLUMNS` reports the key the same way MySQL does. Only a leading column
carries one: `UNI` when a single-column unique index makes that column unique,
`MUL` otherwise — so the leading column of a multi-column unique key is `MUL`,
not `UNI` — and a later column carries nothing. A declared `PRIMARY KEY` or
`UNIQUE` outranks both. All measured.

A `PRIMARY KEY (a, b)` is kept as a composite key. Measured on 8.4.11, MySQL
prints every key column as `NOT NULL` even when its declaration did not specify
nullability; the stored definition, `SHOW COLUMNS` and insertion checks now
agree. A key column explicitly declared `NULL` or `DEFAULT NULL` remains
refused, as do composite keys containing an `AUTO_INCREMENT` column.

An inline `KEY name (column)` inside `CREATE TABLE` is taken. The engine has no
inline non-unique index, so one MySQL statement becomes a `CREATE TABLE` and one
`CREATE INDEX` per key, and they run inside one transaction so the statement
applies whole or not at all: a key naming a column the table does not have
leaves no table behind. `KEY` and `INDEX` are both taken, and both print back as
`KEY`, which is what MySQL does.

A `UNIQUE` key written there is taken the same way, becoming a `CREATE UNIQUE
INDEX` under the same all-or-nothing transaction. That is the shape MySQL prints
for every unique index, so it is what a dumped schema carries and what a
migration writes for a column that may hold one row's value only once.

Measured on 8.4.11 and matched: `UNIQUE KEY uq (e)` and `UNIQUE INDEX uq (e)`
both make an index called `uq`, `CONSTRAINT uq UNIQUE (e)` makes one called `uq`
as well — the constraint's name being the index's where no other is written —
and an unnamed `UNIQUE (a)` is named after its first column by the same rule an
unnamed `KEY` is, counting the names already taken. All of them print back as
`UNIQUE KEY`, in the order the statement wrote them, beside the plain keys. A
table that counts its own ids carries one too. `DEFERRABLE` and `NULLS [NOT]
DISTINCT` are refused, neither being MySQL's spelling.

An unnamed key is named the way MySQL names one: after its first column, and
where that name is taken it gains `_2`, `_3` and so on until one is free. The
names it counts as taken are the ones the statement wrote and the ones it named
before, in the order they were written — measured on 8.4.11,
`KEY (a), KEY (a), KEY (a, b), KEY (b)` names the four `a`, `a_2`, `a_3` and
`b`, and `KEY c_2 (d), KEY (c), KEY (c)` names the three `c_2`, `c` and `c_3`.
An `ALTER TABLE ... ADD INDEX (c)` is named the same way, counting the names the
table already carries.

Refused are the index options MySQL takes there — `USING BTREE`, a prefix
length, `DESC`, `COMMENT`, `INVISIBLE` — since none of them could be printed
back. A key naming a column that does not exist answers 1235 where MySQL
answers 1072.

Index names are now scoped to each table as in MySQL. New indexes use a
unique physical name in the engine and keep the written name in schema
output. Existing indexes with unqualified physical names remain readable. A
child foreign key gets a supporting index when no existing key has its columns
as a left prefix. MySQL's measured rules are followed when an explicit key
replaces that automatic index and when a drop would leave a foreign key without
a supporting key.

On reopen, a legacy table with a foreign key but no index covering its child
columns is rejected with a migration error. Rebuild or re-import it through
the current MySQL frontend. Column-position changes on a child table preserve
its foreign key and supporting indexes through the rewrite and reopen.
Column-position changes on a table referenced by a foreign key are refused:
rewriting that table would retarget the child constraint to the temporary
table name.

A `JSON` column accepts `DEFAULT NULL`, but a literal default is rejected with
MySQL error 1101. A direct index on a `JSON` column is rejected with error
3152. These checks run for `CREATE TABLE` and the supported `ALTER TABLE`
forms, so accepted DDL does not silently retain a default or index MySQL
would refuse.

A new MySQL text column uses the engine's fixed `MYSQL_UCA9_AI_CI` collation,
which follows the primary weights of Unicode 9.0.0 used by MySQL 8.4's
`utf8mb4_0900_ai_ci`. It ignores case and accents, expands characters such as
`ß` for equality, and keeps trailing spaces significant (NO PAD). The same
stored collation is used by comparisons, indexes, unique keys, `DISTINCT`,
`GROUP BY` and ordering. Bare column expressions inherit the stored collation;
an explicit `COLLATE` can override it. The weight table is frozen so a runtime
ICU upgrade cannot change persisted index order. Differential checks against
MySQL 8.4 covered all 29,809 explicit Unicode 9 code points, all 11,172
Hangul syllables, 5,000 implicit-weight code points, 868 contraction sequences
and mixed-string examples.

Fresh table schema envelopes use v3 to record that collation meaning. Existing
v1/v2 envelopes can contain text indexes built under `NOCASE`; they cannot be
reinterpreted as UCA9 without rebuilding the stored data and indexes. Opening
such a legacy text table fails closed with a migration error. No automatic
migration is provided. Existing non-text tables continue to open. A v2
`AUTO_INCREMENT` table keeps its allocator identities when its envelope is
rewritten.

A `VARBINARY` or `BLOB` column has no text collation. An explicit
`utf8mb4_bin` comparison, order or text column declaration uses fixed byte
order with PAD SPACE, so trailing U+0020 spaces compare equal. Nonbreaking
spaces and NUL bytes remain significant.

The frontend rerenders statements that need column types for bound text values
or bare-column ordering. The type check accepts a string parameter only where
the rendered SQL uses the text collation, and refuses string-against-integer
coercions that would otherwise return different rows.

A comparison against a column that is neither an integer nor text runs too, and
for the same reason the frontend can be sure of it: these columns hold the
canonical form MySQL stores, whatever the value was written as. A `DATE` holds
`2024-01-01` however it arrived, so `WHERE d = '2024-01-01'` compares what is
stored against what was written and finds exactly the rows MySQL finds — every
one of them, including the rows written as `'2024-1-1'` or `'20240101'`. A day
and a moment are held zero-padded and widest part first, so reading two of them
in order is reading them in time order, and `<`, `>` and the rest work as well
as `=`. A `YEAR` holds the number it names and a `DECIMAL`, `DOUBLE` or `FLOAT`
holds a number, so an integer compared against one is compared as a number, the
way MySQL compares it.

What is refused is a value written any other way, because rewriting it is a
second rendering pass this does not make. Measured on 8.4.11: `d = '2024-1-1'`
finds the first of January there and comparing the stored form against that text
would find nothing, so it is refused rather than answered differently. So is a
day compared against a `DATETIME` — MySQL reads `'2024-01-01'` as that day's
midnight — and a short year, since `y = 24` finds 2024 there. A `TIME` is the
one of these whose order does not survive: it runs past a day, so its hours
outgrow two digits, and it carries a sign, which puts `-01:00:00` and
`100:00:00` in the wrong place. Only sameness is answered for one. And a `?`
against any of these is refused, because a bound value is not put into the form
the column holds.

An `ENUM` and a `SET` are the same kind of thing. A member is held under the
spelling it was declared with, and MySQL refuses two members that differ only
by case, so a word spelled the way one member is spelled is that one member and
no other — `WHERE state = 'active'` finds what MySQL finds. A `SET` holds its
members joined by commas in the order they were declared, so a subset written
that way is found too. Only sameness is answered: measured on 8.4.11, MySQL
reads an `ENUM` by the position its members were declared in, which is not the
order their words read in. A member spelled some other way is refused —
`state = 'ACTIVE'` finds the row in MySQL, whose collation ignores case, and
comparing the stored spelling against that text would find nothing — and so is
a number, which MySQL reads as a member's position.

A number written with a fraction or an exponent is read too, and meets any column that holds
a number. Measured on 8.4.11: `n > 1.5` over an `INT` answers the rows above one and
`n = 2.0` answers the row holding two, which is what comparing them as numbers answers, so
the literal is carried into the rendered SQL exactly as it was written. A run of digits too
long for an `i64` is still refused rather than read as the nearest number it names: it was
written as a whole number and answering a different one would be worse than refusing. And a
fraction against a text column is refused, the mirror of a string against an integer one —
measured, MySQL reads the text as a number there and raises a truncation warning for a row
that is not one.

A reading of the moment the statement runs is read on the right of a comparison too, which
is how a test asks for today's rows. `CURDATE()` and `CURRENT_DATE` answer a day,
`NOW()` and `CURRENT_TIMESTAMP` a moment, and `CURTIME()` and `CURRENT_TIME` a time of day —
each in the form the column it meets holds, which is what makes `WHERE d >= CURDATE()` the
comparison MySQL makes. Each meets that column and no other: a day against a `DATETIME` is
refused for the reason a written day is, since MySQL reads it as that day's midnight. The
engine reads all three in UTC, which is the one zone these sessions run in.

`UTC_DATE()`, `UTC_TIMESTAMP()` and `UTC_TIME()` read the clock in UTC, and `SYSDATE()` reads
it as the call runs rather than as the statement began. Measured on 8.4.11, each reports the
shape its relative reports — a NOT NULL `DATE` of 10, `DATETIME` of 19 and `TIME` of 8 — and
this server's clock reads UTC, so each is read wherever `CURDATE()`, `NOW()` and `CURTIME()`
are, and answers what they answer. A session in another zone is refused all four, as it is
`NOW()`: MySQL writes a UTC reading into a `TIMESTAMP` as a moment of the session's own zone,
which this does not convert. The bare `UTC_TIMESTAMP` spelling, without parentheses, is read
as a column name.

`LOCALTIME` and `LOCALTIMESTAMP`, with or without parentheses, are two more spellings of
`NOW()`, measured on 8.4.11 to report the same NOT NULL `DATETIME` of 19, and are read
wherever `NOW()` is. Each moment reading and each time-of-day reading takes a count of places
for the fraction of a second, from 0 through 6 — MySQL refuses 7 with 1426 — and as a result
column reports that many decimals and is that much wider, one more for the point: `NOW(6)` a
`DATETIME` of 26 and `CURTIME(3)` a `TIME` of 12. The engine's clock reads to the millisecond,
so a reading asked for four places or more answers zeroes past the third — a reading MySQL's
own clock could have taken, at a coarser grain — and the fraction is cut rather than rounded,
as MySQL cuts it. A reading carrying a fraction is taken as a result column and as a value an
`INSERT` writes, not in a comparison or a shift.

The moment read to a count of places — `NOW(6)`, `CURRENT_TIMESTAMP(3)`, `LOCALTIMESTAMP(6)` —
is written as a value in a row of `VALUES`, the `SET` form and an upsert clause; Rails 8 stamps
`created_at` and `updated_at` with `CURRENT_TIMESTAMP(6)` on every `insert_all` and
`upsert_all`. The same argument holds: past the third place the reading is zeros, a moment
MySQL's clock could have read. What lands in the column is held to the column's own places the
way a written moment is — measured on 8.4.11, `NOW(6)` into a `DATETIME(2)` rounds to two places
and into a `DATETIME` to the whole second, while `NOW(0)` is the second cut short, and a column
of words takes all twenty-six characters. MySQL reads the clock once for the whole statement,
and so does the engine now, once for each step of a statement as SQLite does, so two columns
one row stamps agree and so do the rows of one statement. Seven places is refused, where MySQL
answers 1426, and so is the time of day read to places, `CURTIME(6)`, as a value.

The same three readings are written as values, which is how a row records when it was made:
`INSERT INTO t (created_at) VALUES (NOW())`, `INSERT ... SET d = CURRENT_DATE`, an
`ON DUPLICATE KEY UPDATE updated_at = NOW()`, and `UPDATE t SET dt = NOW()` all write it.
What lands in the column is then put into the form that column holds, the way a written value
is: measured on 8.4.11, `NOW()` into a `DATE` keeps the day and `CURDATE()` into a `DATETIME`
becomes that day's midnight, and both do here. A moment written into a word is the moment
written out, nineteen characters of it, and one too wide for the column is refused with 1406
the way any oversized value is. One difference: MySQL records note 1292 for the time it drops
going into a `DATE` and this drops it quietly. A moment written into a number is refused here, where
MySQL runs it together into 20260908170430 — reading a moment as a number is a rule of its
own and it has not been measured beyond that one shape.

`UPDATE t SET n = n + 1` counts a column up, which is what a `SET` is most often asked to do.
A column is read in an assignment, and `+`, `-` and `*` over one, nested as deeply as they
are written. Division is not: measured on 8.4.11, `b / 2` over 101 answers 50.5 there and 50
in the engine, so the two would write different numbers. Counting a column past its range is
refused the way any oversized value is, and the row keeps what it had — MySQL answers 1690
for the same statement.

Rails' `increment_counter` counts through a fallback naming its column through
its table, `SET posts.views = COALESCE(posts.views, 0) + 1`. A fallback naming
the column alone was taken already; one naming it through its table is taken
now over a column of whole numbers other than a `BIGINT UNSIGNED`, falling back
on a written whole number, the kinds read on the second reading of the
statement the frontend asks for. Measured on 8.4.11: a NULL counts up to 1 and
5 to 6, and counting past the column's range is 1690, which is refused here the
way `n + 1` is. Such a fallback over any other kind of column, or onto anything
but a written whole number, is refused.

A `?` in that arithmetic is how GORM writes `gorm.Expr("balance - ?", 10)`, and MySQL reads
what binds there by the column the answer is written into. Measured on 8.4.11 with the binary
types go-sql-driver sends: into a `DECIMAL(10,2)`, a bound whole number and a bound word
naming a number are taken exactly and the answer rounded half away from zero, so 90.50 less
`'0.005'` stores 90.50 and changes nothing; a bound double makes the arithmetic a double's, so
1.97 plus 0.145 stores 2.11 where exact arithmetic stores 2.12. Into a `BIGINT`, a bound whole
number is added exactly and a double or a word naming a fraction is added as a double and
rounded into the column, 8 plus 1.5 storing 10. A word naming no number fails the statement
with 1292 either way, and a NULL with 1048. So a bound whole number, a word naming one, and
NULL are taken against either column, a word naming a decimal against a `DECIMAL` as well,
and everything else is refused when the statement runs; a `?` in arithmetic written into any
other kind of column is refused when it is prepared. Every `?` in a `SET` holds its own place
among the statement's parameters: they used to be counted from the `WHERE` alone, so an
`ON UPDATE CURRENT_TIMESTAMP` column compared the second of two bound values against the
first's and missed the change.

`SET ratio = score / 2` scales a column down, and MySQL's `/` is decimal division where the
engine's is integer division. What lands in the column is rounded to the column's own scale on
the way in, which is what makes the two agree: measured on 8.4.11, 10 divided by 3 into a
`DECIMAL(10,2)` is 3.33, 5 by 2 is 2.50, and a scaled column halved is 1.50, all of which this
now writes.

The divisor has to be a written number that is not zero. Dividing by zero answers NULL in the
engine where MySQL raises 1365 for a write, and only a written divisor says which of the two a
statement would get. A fraction written into a whole-number column is refused as well: MySQL
rounds it into the column and the engine will not store it. One that divides evenly writes the
number MySQL writes — measured, 20 halved is 10 in both.

One shape is refused for a reason worth knowing. MySQL reads the columns a `SET` has already
assigned in the values after them: measured, `SET a = 100, b = a` leaves `b` at 100, where the
engine reads the row as it was and would leave it at the old `a`. So a value naming a column
the same statement has already assigned is refused. The other order, `SET b = a, a = 100`,
reads nothing that was assigned and is answered.

A `LIKE` pattern escapes its own `%` and `_` with a character, and where the statement names
none MySQL takes a backslash — unless the session runs with `NO_BACKSLASH_ESCAPES`, which
leaves the pattern with no escape at all. The engine has no escape of its own and takes the
`ESCAPE` clause, so the clause is written to say what MySQL would have taken, and the session's
mode is what decides whether there is one to write.

Measured on 8.4.11 over `a_b`, `axb`, `a%b`, `ab` and `a\b`, and matched: an escaped
underscore matches the one row spelling it, a bare one matches every three-character row, an
escaped percent matches the row holding one, an escape before an ordinary letter is dropped by
both, an escaped escape matches the row holding a backslash, and an escape the statement names
works the same way. A bound pattern carries the clause too, so a value bound into one is read
the way MySQL reads it.

MySQL renames a table with a statement of its own as well as with an `ALTER TABLE`, and a
migration writes whichever its tool generates. The words are moved into the `ALTER TABLE`
shape, so one reader answers both and the names may be quoted as a generated statement writes
them. Renaming several tables at once is refused: MySQL renames them together, and several
`ALTER TABLE`s would not. A name already taken and a table that is not there are refused here
where MySQL answers 1050 and 1146, which the `ALTER TABLE` spelling has always done.

MySQL spells dropping an index two ways — `DROP INDEX name ON table` and
`ALTER TABLE table DROP INDEX name` — and a migration writes whichever its tool generates. The
second was already read, so the first is written into that shape and one reader answers both.
Measured on 8.4.11 and matched: both drop the key, the names may be quoted, and an index that
is not there is 1091. The engine's own `DROP INDEX name` names no table and is refused, MySQL
requiring one.

A catalogue read may write its database out rather than asking for the selected one with
`DATABASE()`, which is what a migration tool that knows the database it is working on writes.
This server has the columns of the selected database alone, so any other name answers no rows.
The name is read as it was written: measured on 8.4.11, `TABLE_SCHEMA = 'TURSO_ORACLE'`
answers nothing where `'turso_oracle'` answers the columns, and both are matched here.

`WHERE active` is how a statement tests a column that holds a flag, which MySQL reads as a
comparison against zero and the engine reads the same way, so the column is written out as it
stands. Measured on 8.4.11 over a `TINYINT(1)` and an `INT` and matched: a value that is not
zero keeps the row, zero and NULL do not, a negative number keeps it, `NOT` turns the test
around without letting NULL through, two columns combine with `AND`, and a column named with
its table reads the same.

A column of words is refused. MySQL reads one as the number it begins with — measured, every
word in a column of them tested false — where the engine compares a word against a number by
their kinds, so the two would keep different rows. Knowing which kind the column is takes the
second rendering pass an `ORDER BY` over a bare column already asks for, which an `UPDATE` and
a `DELETE` have not got; a bare column tested in one of those is refused.

`IFNULL(email, 'none')` is how a report writes a placeholder for what a row does not carry, and
`COALESCE` is the other spelling of it. Measured on 8.4.11 and matched: the answer is the
column's own width whatever the word's own is — over a `VARCHAR(80)` both `'none'` and `'x'`
report a `VAR_STRING` of 320 — and it is NOT NULL, a written word being there whether the
column is or not. A `CHAR(5)` reports 20, and a `VAR_STRING` rather than the `STRING` it
reports on its own.

A `TEXT` column is refused: measured, MySQL reports four times its own width there, which is a
rule of its own. So is a word falling back onto a column of numbers, which is a coercion.

`ON t.id = u.team_id AND t.name = 'red'` is how a statement narrows the side it joins to, and
an `ON` says more than which columns to match on. Matching one column against another is what
a join is for and is rendered as such; everything else the `ON` says is a comparison against a
value and goes through the reader a `WHERE` comparison goes through, so the value is held to
the column's own type the same way — a word against a column of numbers is refused there as it
is in a `WHERE`.

Measured on 8.4.11 and matched: a word on the side joined to keeps the rows whose team is that
one, a number on the statement's own side keeps the rows that answer it, an outer join
narrowed the same way keeps every row on the left and answers NULL for the side that missed, a
comparison that is not equality narrows the same way, and three conditions combine. An `ON` in
an `UPDATE` or a `DELETE` still takes columns alone: each renders its own `FROM` with no
statement to record the comparison in, so a value there would go unchecked.

`ORDER BY LOWER(name)` is how a report asks for an order it has worked out rather than one a
column holds, so any call whose shape is already known is ordered by. What it answers is
collated the way a text column is, which is also right for a number: a collation says nothing
about one in the engine. Measured on 8.4.11 over 'beta', 'Alpha', 'alpha', 'Zulu' and 'apple'
and matched: `LOWER`, `UPPER` and `CONCAT` each order the rows the way the bare column does,
and `LENGTH`, `ABS` and `DATE` order by what they answer.

`ORDER BY RAND()` is refused. It orders the rows by nothing a client can hold this to, and
whether each engine reads a random number once or once a row is a rule of its own.

`FROM (SELECT ...) x` is a whole statement standing where a table does, which a query writes to
narrow rows before the outer statement reads them. It reads its table under the alias it was
given, which is how its result columns find their metadata — the same way a CTE's do, and for
the same reason: a result column reaching the frontend names the alias and an ordinal, and the
only way to answer what type it has is to read that ordinal from the table the body reads.

Measured on 8.4.11 and matched: the columns read back under the alias carry the table's own
shapes, an `INT` reporting a `LONG` of 11 and a `VARCHAR(40)` a `VAR_STRING` of 160; the body
may narrow its rows; the columns may be read back in another order than the body projected
them; a name needs no alias when only one source answers to it; and a derived table with no
alias is 1248, as MySQL requires one.

The body reads one table, or joins tables as a later paragraph says. What it projects decides what each column is, and the same holds for
a CTE's body:

- A column of the table, under its own name or an alias. `(SELECT u.id AS uid FROM users u) x`
  answers `x.uid` as `users.id`, and TypeORM's pagination query — `SELECT DISTINCT
  distinctAlias.User_id AS ids_User_id FROM (SELECT User.id AS User_id FROM users User)
  distinctAlias ORDER BY User_id ASC LIMIT 10` — reads that way.
- `*`, which reads the table column for column: `WITH active AS (SELECT * FROM users WHERE
  status = 'active') SELECT * FROM active`.
- In a body that aggregates, a `COUNT`, a `SUM`, an `AVG`, a `MIN` or a `MAX` over a column, or
  a `DATE` it groups by — `(SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id) t`. An
  unaliased one goes by its source text, `COUNT(*)`.

Measured on MySQL 8.4.11, MySQL reads the two kinds of body differently, and the shapes follow
it. A body that aggregates is written out into a table of its own first, and each column is that
table's: a column of the table keeps its NOT NULL, its default and its sign but none of its keys
and no auto-increment, and an answer the body worked out names the derived table, goes by its
name in the body and names no database and no original table, with every number losing the
binary flag — a count, a total, a largest and an average alike — words losing the 31 decimals a
call's words carry, and a day and a moment keeping their binary flag. Any other body is read
straight through: every column keeps every flag, and a day, a moment and a time of day come back
in the connection's character set, four bytes to each character they spell — a `DATETIME` 76
where on its own it reports 19 — and a JSON column in it too. Either way a column of the table
goes by the name the body gave it and names the table under the name the body read it under,
its alias when there is one: TypeORM's `ids_User_id` names `User` and `User_id`. On the outer
side of a `LEFT JOIN` an answer the body worked out is nullable, as MySQL reports it: Prisma's
relation count, `LEFT JOIN (SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id) AS t`,
reads NULL for a user with no posts, and the count reported NOT NULL there used to be wrong.

A comparison the outer statement makes on a derived column is held to what the column is: a
column of the table to its declared type under the body's name for it, a count to a whole
number, and a largest or smallest to its column's kind — `WHERE t.c > 0`. A total or an average
compared against a value is refused, having not been measured. So are an expression in a body
that does not aggregate, which MySQL reads straight through and whose shape there has not been
measured; a `DISTINCT` body; two columns going by one name, which MySQL answers 1060 for; an
aggregate over a column the body worked out; a `LATERAL` one; one naming its own columns; and one
in an `UPDATE` or a `DELETE`, each of which reads its own table. A name the body gives a column
that is also another of the table's columns is refused where the statement needs the columns'
types, since it would stand for two.

A `DISTINCT` over a derived table or a CTE reading one table is taken only when it reads the
table's single-column primary key and nothing else, ordered by that key first — TypeORM's
`SELECT DISTINCT distinctAlias.User_id AS ids_User_id FROM (...) distinctAlias ORDER BY User_id
ASC LIMIT 10`. Measured on 8.4.11, MySQL then reads the key in order and the column is read
straight through; any other `DISTINCT` there is written into a table of MySQL's own — the
table's own name, the result column's name and no database, a nullable column carrying the
32768 flag — unless an index serves the column, so it is refused, where it used to be answered
with the shapes read straight through.

A body may also join tables: a first table and `LEFT JOIN`s, each matched `ON` columns, every
column named with its table — TypeORM's pagination over an entity loaded with its relations,
`SELECT DISTINCT distinctAlias.Post_id AS ids_Post_id, distinctAlias.Post_id FROM (SELECT
Post.id AS Post_id, ..., Post__Post_tags.name AS Post__Post_tags_name FROM posts Post LEFT JOIN
post_tag ... LEFT JOIN tags Post__Post_tags ON ...) distinctAlias ORDER BY distinctAlias.Post_id
ASC, Post_id ASC LIMIT 10`. Each column is traced to its own table. Measured on 8.4.11 and
matched over the text and the binary protocol, MySQL reads such a body two ways, and neither
depends on how many rows the tables hold — none, one each, or thousands:

- The statement's own `DISTINCT` makes MySQL write the rows into a table of its own, and each
  column reports that table's column: the original table is the column's own table under its
  own name (`posts`, not the alias `Post`), the original name is the result column's
  (`ids_Post_id`), no database is named, and the keys and the auto-increment are gone, while
  the type, length, decimals, character set and every other flag are the table column's — a
  moment stays a `DATETIME` of 19, a document a `JSON` of 4294967295 in the binary collation.
  A `LIMIT` and an `ORDER BY` among the projected columns — by name, by the name the statement
  gives, or by place — may follow; anything else in the `ORDER BY` is MySQL's 3065.
- Without it the body is read straight through, as a body reading one table is: every flag
  stays, the original table is the body's alias for the column's table and the original name
  the body's name for the column, and a day, a moment or a document is reported in words.

Either way a column on the side a `LEFT JOIN` can leave missing loses its NOT NULL, and a
`DISTINCT` keeps the first of the words it counts as one, `Alpha` before `alpha`, as MySQL does.
TypeORM's next two statements already worked: the page's posts read by id —
`... WHERE Post.id IN ('1', '2') ORDER BY Post.id ASC` over the same joins — and the count,
`SELECT COUNT(DISTINCT Post.id) AS cnt FROM posts Post LEFT JOIN ...`.

Refused over a body joining tables, each measured or not measured: a condition in the body —
measured, `WHERE Post.id = 1` makes MySQL read `posts` as one constant row, whose columns then
report the table's own shapes, and a derived table's again when no such row exists; a value in
a join's `ON`, only columns matched against each other having been measured; an inner join,
whose order MySQL picks itself; an `ORDER BY` without `DISTINCT` — measured, MySQL sorts
through a table of its own when it matches a joined table by hash rather than by a key, which it
chose for a table of two rows that had a key; a `LIMIT` with no order; and anything but the
derived table's own columns — a condition, a grouping, a count, an expression, `*`, a second
table, or the derived table inside another statement. A statement needing its columns' types
refuses a name two joined tables hold in different kinds, as a subquery's does.

A statement that only counts the rows of its one derived table — `SELECT COUNT(*) FROM
(SELECT 1 AS one FROM posts LIMIT 3 OFFSET 0) subquery_for_count`, which is how Rails counts a
relation carrying a limit — reads no column of the body, so the body may project anything,
written values included, and may cut its rows with a `LIMIT` of written numbers. Measured on
8.4.11, it answers how many rows the limit leaves, 3 of 3 and 1 past an offset of 2, in the shape
a plain `COUNT(*)` answers, and which rows a limit without an order keeps does not change how
many it keeps. An `ORDER BY` in such a body and a bound row count are refused, and a statement
reading anything more of the derived table is held to the rules above.

A shift by months keeps the day inside the month it lands in. MySQL takes the last day of the
target month where that month has no such day, and the engine's own month arithmetic overflows
into the next one instead: measured on 8.4.11, `2026-01-31` a month on is `2026-02-28` where
the engine answers `2026-03-03`. That is a different day rather than a missing feature, so the
whole shift is worked out by this frontend and the rendered SQL calls it. Measured and matched
row by row: three months on from that day is `2026-04-30`, a month back from `2026-03-31` is
`2026-02-28`, and `2024-02-29` a year on is `2025-02-28`.

A week and a quarter are counted in the unit each is made of — measured, a week is exactly
seven days and a quarter exactly three months, `2026-01-31` a quarter on and three months on
both answering `2026-04-30`. A negative count shifts the other way, as `DATE_SUB` does. A day
alone stays a day when the shift is by whole days and becomes a moment at midnight when it is
not, which is what MySQL answers. A shift counts a written number and nothing else: a count
worked out from a row cannot be multiplied for a week or a quarter.

A fraction of a second is carried through a shift: measured, a `DATETIME(3)` holding
`2024-01-31 10:20:30.250` a day on answers `2024-02-01 10:20:30.250`, and the shift reports
the column's own decimals — a `DATETIME` of 23 with 3 decimals, from a `DATETIME(3)` and a
`TIMESTAMP(3)` alike. Before, the fraction was dropped, or rounded into the next second when it
was a half or more, and the shift reported 19 with none.

`created_at + INTERVAL 1 DAY`, `INTERVAL 1 DAY + created_at` and `created_at - INTERVAL 1
HOUR` are MySQL's operator spellings of `DATE_ADD` and `DATE_SUB`, and each is read as the call
it is, wherever the call is read: in a projection, on the right of a comparison —
`WHERE created_at > NOW() - INTERVAL 1 DAY` — and as a value an `INSERT` or an `UPDATE`
writes, `SET expires_at = NOW() + INTERVAL 30 MINUTE`. Measured on 8.4.11, the two spellings
answer the same moment and the same shape, and MySQL names an unaliased column after the
whole of the operator form, `INTERVAL` and unit included. Two shifts in a row — `created_at +
INTERVAL 1 DAY + INTERVAL 1 HOUR` — are a shift of something other than a moment, and are
refused.

A moment written out as a word is shifted too: `'2026-01-31' + INTERVAL 1 MONTH` is
`2026-02-28`. Measured, MySQL answers a word here rather than a moment — a `STRING` of 116 in
utf8mb4 with no flags — keeps a day written alone a day, `'2026-1-1' + INTERVAL 1 DAY` being
`2026-01-02`, and writes a written fraction out to six places, `.5` as `.500000`. Only the
spelling with dashes, a space and colons is taken: MySQL reads many others, and which of them
it takes for a day alone has not been measured. A word naming no moment is refused, MySQL
answering NULL for it.

`CONCAT(name, '-', id)` is how a query builds a label out of a row, so the call takes a number
as readily as a word. Its answer is as wide as its arguments laid end to end, measured on
8.4.11, and a number spells as many characters as its type does rather than as many as its
column reports: a `BOOLEAN` column reports one and spells four, being a `TINYINT` under the
display width MySQL keeps for it. A moment, a day, a span of time and a year are spelled the
way they are stored, so those are taken too.

A `DECIMAL`, a `FLOAT` and a `DOUBLE` are refused. Their conversion to text
inside `CONCAT` needs MySQL's numeric formatting rules; the direct result
rendering of an exact `DECIMAL` does not establish those generic conversion
rules. A `FLOAT` holding a third spells `0.333333`, and a `DOUBLE` holding
12345678901234567890 spells `1.2345678901234567e19` in MySQL.

`SELECT team_id, COUNT(*) AS c FROM t GROUP BY team_id HAVING c > 1` is how a grouped report
names its own answer, and a name in a `HAVING` is read as the projection's alias before the
table's column. Measured on 8.4.11: the alias wins even when the table carries a column of the
same name, so the names are resolved to what they stand for before the clause is read at all —
what a name stands for is also what decides whether the clause filters rows or groups. An
alias over the grouped column filters on the grouping, two aliases combine with `AND` and `OR`,
an alias with no `GROUP BY` filters the one implicit group, and an aliased column with no
`GROUP BY` keeps filtering rows the way the unaliased spelling does. A name no alias and no
projection answers to is 1054, as it is there.

`CASE WHEN n > 15 THEN 1 ELSE 0 END` is how a query answers a flag, and `IF(n > 15, 1, 0)` is
the call spelling of the same thing. Both are taken, in a projection and in a `SET` alike, so
`UPDATE t SET active = CASE ... END` writes what the same branches read. The answer's shape is
a rule over its branches rather than a type of its own, measured on 8.4.11: over written
numbers, a `LONGLONG` as wide as its widest branch plus one for the sign — `THEN 1 ELSE 0` reports 2, `THEN 100 ELSE -5`
reports 4, `THEN n ELSE 0` over an `INT` reports 11, which is the `INT`'s own ten digits and
the sign, and `IF(c, n, big)` reports 20. It carries the binary and numeric flags and no
decimal places, and is NOT NULL only when every branch is and there is an `ELSE` for a row to
fall to — measured, a `CASE` with no `ELSE` is nullable whatever its branches hold.

A branch is a written number, a written word, a column or `NULL`. The answer is the kind every
branch shares and as wide as the widest, measured on 8.4.11 over both protocols:

- Whole numbers answer the widest integer type among them, a written number counting as a
  `BIGINT`, as long as the longest: `THEN age ELSE small` over an `INT` and a `SMALLINT` is a
  `LONG` of 11, `THEN small ELSE tiny` a `SHORT` of 6, `THEN 5 ELSE small` a `LONGLONG` of 6.
  A `TINYINT(1)` counts as the `TINYINT` it is, 4 wide.
- A `DECIMAL` beside whole numbers or other `DECIMAL`s answers a `NEWDECIMAL` with the most
  digits before the point and the most after it: a `DECIMAL(10,2)` beside an `INT` reports 14
  with 2 places, beside a `DECIMAL(6,3)` 13 with 3. Each row answers its branch at that
  branch's own scale, not the answer's: `THEN balance ELSE 0` answers `10.50` and `0`, and
  `THEN fee ELSE balance` answers `1.125` and `0.00`. Each branch is written out as the text
  it is, so the engine answers the same.
- A `DOUBLE` beside any number answers a `DOUBLE` of 23 with not-fixed decimals, and a whole
  number branch comes back as a double — `0` over the text protocol, 0.0 over the binary one.
- Words, and `VARCHAR` and `CHAR` columns of one collation, answer a `VAR_STRING` four bytes a
  character of the longest: `THEN name ELSE 'minor'` over a `VARCHAR(100)` reports 400, and a
  `CHAR(5)` beside a `VARCHAR(30)` 120.

`CASE col WHEN 'a' THEN ...` is written as `CASE WHEN col = 'a' THEN ...`, so each comparison
is checked and collated the way a `WHERE` comparison is: measured, `CASE status WHEN 'ACTIVE'`
matches `active`. MySQL compares the operand by one rule chosen over every `WHEN` value
together, which is the rule each comparison chooses on its own only when the values are all of
one kind, so they have to be all written words or all written whole numbers, and the operand a
column.

`IFNULL(nickname, name)` and `COALESCE(a, b, c)` over columns alone answer what a `CASE` over
the same columns answers, and are NOT NULL when any one of the columns is. One thing differs,
measured: a `DECIMAL` answer brings every value to its scale, so `IFNULL(age, balance)` answers
`30.00` and `COALESCE(fee, balance)` `7.000`.

`SUM(CASE WHEN status = 'active' THEN 1 ELSE 0 END)` is how a report counts the rows meeting a
condition, and `COUNT`, `SUM`, `AVG`, `MIN` and `MAX` are taken over a `CASE` or an `IF`.
Measured: `COUNT` answers what any `COUNT` answers; `SUM` answers a `NEWDECIMAL` 22 digits wider
than the `CASE` at its scale — 24 over `THEN 1 ELSE 0`, 34 with 2 places over a
`DECIMAL(10,2)` — and `AVG` one 4 digits and 4 places wider, `0.7500` over the same flags. Both
answer at the `CASE`'s scale whatever the rows held: `SUM(CASE WHEN ... THEN balance ELSE 0
END)` over no matching row is `0.00`, so each value is brought to that scale before it is
added. Over a `DOUBLE` all of them answer a `DOUBLE`. `MIN` and `MAX` answer the `CASE`'s own
shape and are taken over whole numbers and doubles.

Refused, each answering by a rule not measured here: a word beside a number, which is a
coercion; an unsigned column beside anything — a `BIGINT UNSIGNED` beside a written number
answers a `NEWDECIMAL`, a `TINYINT UNSIGNED` beside a `TINYINT` a `SHORT` of 4; a `TEXT`, a
`FLOAT` and a column holding a moment; a written number carrying a scale — `THEN 1.5 ELSE 0`
answers a `NEWDECIMAL` by a rule of its own; `MIN` and `MAX` over a `DECIMAL` or words, which
the engine would compare as the words they are written as; ordering by an `AVG` over a `CASE`,
for the same reason; a column read through a join, whose type this reads off one table only;
and `IFNULL` or `COALESCE` over a column and anything else but a written number or word, and
any of the new forms written into a column by an `UPDATE`.

One difference: grouped by a `GROUP BY`, MySQL drops the binary flag from `SUM`, `COUNT`, `MIN`
and `MAX` and keeps it on `AVG`; this reports the flag as it does for an ungrouped aggregate,
as it already does for an aggregate over a plain column.

An integer column takes the display width a dump or an ORM writes it with —
`id INT(11)`, `active TINYINT(1)`, `n BIGINT(20) UNSIGNED` — which is the spelling most real
schemas carry, and without it a schema does not land at all. The width says how wide a client
should print the number and nothing about what the column may hold, and MySQL 8.4 deprecated
it and drops it: measured on 8.4.11, all three read back with no width, `INT(3)` still holds
every `INT`, and a `TINYINT` still refuses 200 with 1264. So the width is taken and dropped
here too.

`TINYINT(1)` is the one width MySQL keeps, because a client reads it as a boolean, and it is
exactly what MySQL stores `BOOLEAN` as — measured, both print `tinyint(1)` and both report a
length of 1 where a plain `TINYINT` reports 4. The two spellings meet on one stored type here.
`TINYINT(1) UNSIGNED` is not one of them and reads back as `tinyint unsigned`, which is what
MySQL prints for it. One difference: MySQL raises warning 1681 for each width it drops and
this raises none, so a client counting warnings after a `CREATE TABLE` sees zero here.

A fixture writes its own ids — `INSERT INTO t (id, name) VALUES (1, 'a')` — and a counted
table takes them, and so does a prepared statement binding them, which is how Prisma writes
every row. The counter never hands the same number out again: where a `VALUES` row names an id
past it, the rows are written under the counter's lease and it moves past each row's number
once that row is written. Measured on 8.4.11 and matched: an id of 20 refused as a duplicate of
another key leaves the next number where it was, while a row asking for the next number and
refused as a duplicate spends it, one row or several, in a transaction or out of one — so a
row that fails as Laravel's `User::create` of a taken email does leaves the next user one
number on. The other forms, `INSERT ... SET id = 1` and rows a trigger numbering a counted table
of its own sets off, raise the counter before the row is written. Measured on 8.4.11 and matched: rows
written out of order still leave the counter one past the highest of them, a written id below
the counter leaves it where it stands, and a negative id is stored as written and moves
nothing. `LAST_INSERT_ID()` is left as it stood, which is what MySQL does — but the id the
statement reports to its client is the last row's written value, a different number from the
one the counter moved past when the rows descend. MySQL's `INSERT ... SET id = 1` form writes
the same row and is taken the same way, and writing an id that is already there is the
ordinary collision, 1062.

Writing a 0 or a NULL into that column asks for the next number rather than naming one, which
is exactly what leaving the column out asks for, so a statement whose every row asks that way
is numbered as if the column had not been written at all. That is the spelling a fixture and a
legacy `INSERT` both use. Measured on 8.4.11 and matched: `VALUES (NULL, 1)` into an empty
table writes 1, `VALUES (0, 2)` after it writes 2, and a statement whose rows all ask reports
the first of the numbers it took — `VALUES (NULL, 3), (0, 4)` reports 3.

A statement mixing a row that names its own number with one that asks stays refused. The
counter is raised once for the whole statement here, and MySQL moves it row by row: measured,
`VALUES (NULL, 6), (50, 7), (NULL, 8)` writes 6, 50 and 51, so the third row's number depends
on the second row's. One range reserved before the statement runs cannot answer that.

`INSERT INTO t VALUES (...)` with no column list means every column of the table, in order,
and it is how mysqldump writes every data row, so a dumped table's rows arrive in exactly that
shape. The column list is written into the statement here — into it, rather than the statement
being rendered again, because a written value's own spelling is the one thing that must not
change on the way through — and the ordinary path runs. Measured on 8.4.11 and matched: a
plain table counts its rows and reports no id, a counted table's rows carrying their own ids
report the last row's, and a written NULL among them asks the counter, which the written ids
have moved past. `INSERT INTO t () VALUES ()` is a different statement — an empty column list
the statement wrote itself, meaning the row of defaults — and keeps its own path.

The row of defaults is written two ways — `INSERT INTO t () VALUES ()`, and every column the
statement names given `DEFAULT` — and both mean one row where every column takes its own
default. On a table that counts its own ids that row takes the next number like any other.
Both forms render as the engine's `DEFAULT VALUES`, which writes one row and offers nowhere to
put a value, so a row is made for the number at the point where the column it goes in is
known. Measured on 8.4.11 and matched: `INSERT INTO t () VALUES ()`, `VALUES (DEFAULT,
DEFAULT, DEFAULT)` and `(n) VALUES (DEFAULT)` each write one row numbered 1, 2 and 3, every
other column holding its own default, and each reports the number it took. Several rows of
defaults are refused there, the way several rows of anything else on such a table are.

A `JSON` column crosses a prepared statement now. A prepared statement is how every real driver
executes, and the binary row this server writes knew every column type but that one, so a
driver reading a document got an error where a text query got the document. Measured on 8.4.11
and matched: a JSON column crosses the same way over both protocols — the type is `json`, the
character set is binary, and the value is the document's own bytes, length-encoded, which is
what a `BLOB` crosses as. A NULL document crosses as a NULL either way.

A column's own default is read and printed the way MySQL prints it. Two shapes every real
schema carries used to make the whole table unreadable — an `ENUM` with a default, which is
every status column, and a `DECIMAL` whose default is written with a point, which is every
money column. The `CREATE TABLE` was taken and then nothing could read the table back: `SHOW
CREATE TABLE`, `SHOW COLUMNS` and `information_schema.COLUMNS` all refused it.

Measured on 8.4.11 and matched: a column keeps its default at its own scale and prints it
quoted — `DECIMAL(10,2) DEFAULT 3` prints `DEFAULT '3.00'`, `DEFAULT 1.5` on a `DECIMAL(6,3)`
prints `'1.500'`, and a `DOUBLE` prints what was written. `SHOW COLUMNS` and
`information_schema.COLUMNS` report the same number without the quotes, and an `ENUM`'s default
is the word it is. A word default is written back by the rule a column's comment is written by,
which was measured for that: a quote doubled, a backslash written twice.

`DECIMAL` defaults are read as exact decimal values and rounded to the column's
scale. `DECIMAL(10,2) DEFAULT 1.239` prints `'1.24'`, and
`DECIMAL(10,2) DEFAULT '4.5'` prints `'4.50'`.

A column of whole numbers takes a word naming a number as its default, which is
how Laravel writes every default (`->default(0)` and `->default(false)` both
become `DEFAULT '0'`) and how `mysqldump` and `SHOW CREATE TABLE` print one.
Measured on 8.4.11 and matched: the word is read as an exact number and rounded
half away from zero into the column — `INT NOT NULL DEFAULT '5'` prints
`DEFAULT '5'`, `'4.5'` prints `'5'`, `'-4.5'` prints `'-5'`, `' 7'` and `'7 '` print
`'7'`, `'007'` prints `'7'`, `'.5'` prints `'1'`, `'1e2'` prints `'100'` and
`TINYINT(1) NOT NULL DEFAULT '0'` prints `'0'` — and a written number with a point
is rounded the same way, `INT DEFAULT 1.25` printing `'1'`. `SHOW COLUMNS` and
`information_schema.COLUMNS` report the same number unquoted. MySQL answers 1067
for a word naming no number (`''`, `'abc'`, `'5a'`, `'0x10'`) and for a default that
lands outside the column's range once rounded (`TINYINT DEFAULT '300'`,
`'127.5'`, `TINYINT UNSIGNED DEFAULT '-1'`), and this refuses the same
statements, while `INT UNSIGNED DEFAULT '-0.4'` rounds to 0 in both. A `DOUBLE` or
`FLOAT` takes a default MySQL prints back as it was written, `'0'` or `'1.5'`,
and refuses one MySQL would print as some other number (`'1.50'`, `' 1'`,
`'1e2'`) along with a word naming no number, which MySQL answers 1067 for.

`DOUBLE PRECISION`, which Django writes for every `FloatField`, `REAL` and
`FLOAT8` are other spellings of `DOUBLE`, and `FLOAT4` of `FLOAT`; the column
keeps nothing of the spelling. Measured on 8.4.11 and matched: each prints as
`double` (or `float`) in `SHOW CREATE TABLE` and `information_schema.COLUMNS`,
`DOUBLE PRECISION UNSIGNED` and `REAL UNSIGNED` as `double unsigned`. `REAL` is a
`DOUBLE` because MySQL's `REAL_AS_FLOAT` mode is off, which it is by default and
in the one mode this server runs in.

`BIT` and `BIT(1)` hold one bit, which is what Hibernate 6 maps a Java `Boolean`
to on MySQL. Measured on 8.4.11 and matched: `bit` is `bit(1)` in `SHOW CREATE
TABLE`, `SHOW COLUMNS` and `information_schema.COLUMNS` (`DATA_TYPE` `bit`,
`NUMERIC_PRECISION` 1, `NUMERIC_SCALE` NULL); a default is written 0, 1,
`FALSE`, `TRUE`, `b'0'` or `b'1'` and printed as the bit literal, unquoted, in
all three; 0, 1, `TRUE` and `FALSE` are stored and 2, -1 and the word `'1'` are
1406; `c = 1`, `c = TRUE`, `c <> 0` and `c IN (0)` find the rows holding those
bits and `c = 2` finds none. A result column reports the BIT type, a length of
1, the binary collation and the unsigned flag, and its value crosses the text
protocol as the one byte holding the bit, 0x00 or 0x01. The binary protocol
sends the same byte length-encoded, which is the form the protocol gives a
BIT; that one was not measured, no client on the oracle's host speaking it.
The engine holds the bit as the integer 0 or 1.

A column declared `NOT NULL DEFAULT NULL`, in either order, is refused, as MySQL
refuses it with 1067.

`DEFAULT` written where a value goes asks for the column's own default, which is what a
generated `INSERT` writes for a column it has nothing to say about. The engine has no spelling
for it, and leaving the column out of the statement asks for the same thing — measured on
8.4.11, a column left out and a column given `DEFAULT` both take the column's default, both
leave a nullable column with none at NULL, and both answer 1364 when the column is NOT NULL
with no default of its own. So a column given `DEFAULT` in every row is dropped from the
statement, and `DEFAULT(col)` naming that same column is the other spelling of it. Every row
has to agree: leaving the column out would take the default for all of them, so a statement
that writes `DEFAULT` in one row and a value in another is refused. A quoted `` `default` ``
is an ordinary column name, which is what MySQL takes it for.

An `AUTO_INCREMENT` column takes `DEFAULT` too, and counts on from where it stood. The counter
is filled in by this frontend rather than the engine, so it reads the column list the engine
will run rather than the one that was written — a column given `DEFAULT` is already gone by
then, which is the one shape the counter can fill in. Every column of a counted table given
`DEFAULT` is refused: that writes the row of defaults, which leaves the counter no row to put
its number in.

Beside `ON DUPLICATE KEY UPDATE`, the row offered carries the column's own default for a column
given `DEFAULT`, which is what the engine's offered row carries for a column left out: measured
on 8.4.11, `VALUES ('b', 'C', DEFAULT) ... hits = VALUES(hits) + 1` over a column defaulting to
7 writes 8, and naming the offered row reads 7 too. TypeORM's `repository.upsert()` writes
`DEFAULT` for the counted id of every row, `VALUES (DEFAULT, 'go'), (DEFAULT, 'news') ON
DUPLICATE KEY UPDATE name = VALUES(name)`, and it is numbered, counted and reported as the same
upsert with the id left out: measured, a new row reports its number, a colliding row spends
one, a row the clause leaves as it stood counts 0 and one it changes counts 2 and reports its
own id. Reading the counted column off the offered row is refused — MySQL answers the number
the colliding row spent there — and so is an upsert whose every column is given `DEFAULT`,
since the engine's row of defaults has no room for the clause.

`SET n = DEFAULT` on an `UPDATE` is refused. MySQL writes the column's default there, and this
cannot work out what that is from the statement alone.

A `SET` also takes its value out of another table: `SET n = (SELECT MAX(m) FROM src)` writes
the highest number the source holds, and `SET name = (SELECT MIN(word) FROM src WHERE id = 1)`
writes the word a narrowed source answers. What makes a subquery a value is that it answers
exactly one row, which an aggregate over one implicit group does — the same reading a
comparison against a subquery is held to. A plain column does not, and MySQL answers 1242
there, so that is refused. A subquery reading the table being changed is refused too, which is
MySQL's 1093. The column written and the column read are held to the same kind, so a word into
a column of numbers is turned away rather than coerced, and a `COUNT(*)` is refused because a
count says nothing about the kind of the column it would be written into.

`LIMIT 18446744073709551615 OFFSET n` is how MySQL is asked for every row after an offset, and
its row counts run to a whole unsigned 64-bit number where the engine's run to a signed one.
No table holds that many rows, so a limit that wide keeps every row — which the engine spells
as a negative count — and an offset that wide skips every row, which the widest count the
engine reads already does. Measured on 8.4.11 and matched: that limit answers every row after
the offset and every row without one, an offset that wide answers none, the comma spelling
means the same, and one past what a row count holds is 1064. An `UPDATE` or a `DELETE` written
with a count that wide is refused instead, not having been measured.

A row count is written as a parameter as readily as a number, which is what a client that
prepares a paged query writes: `LIMIT ?`, `LIMIT ? OFFSET ?` and MySQL's other spelling
`LIMIT ?, ?` all bind. Each spelling is rendered as it was written rather than turned into
the other, because the engine reads both and means the same by each — and because a client
binds by where the `?` stood in the SQL it wrote, which only holds if the rendered SQL keeps
them in that order. The comma spelling writes the offset first, so that is the parameter
bound first. What a row count binds is held to a whole number that is not negative: the
engine reads a negative one as no limit at all, where MySQL refuses one, so answering every
row would be the wrong answer rather than an error.

A count takes a qualified column, which is what a join has to write. `COUNT(b.id)` beside a
`LEFT JOIN` is how a query asks how many rows each row on the other side has, and a join
cannot leave the qualifier off. A count is the one aggregate this can take qualified, because
it is the one whose result does not depend on what the column holds — measured on 8.4.11, a
count is a non-null `LONGLONG` of length 21 whatever it counts. `MIN`, `MAX` and `SUM` answer
their argument's own type, and over a join the qualifier says which table's column that is:
SQLAlchemy's `sum(posts.views)` beside `users.name`, Laravel's `sum(posts.views)` grouped by
`users.email` and TypeORM's `MAX(Post.views)` over a `LEFT JOIN` are each answered in the shape
the call answers over that table alone — measured on 8.4.11, a total over an `INT` a
`NEWDECIMAL` of 33, over a `BIGINT` one of 42, a largest the column's own type, each nullable
with the binary flag, and NULL for a group the `LEFT JOIN` found nothing for. Only a signed
whole number is taken there: the engine keeps a `DECIMAL` and a `BIGINT UNSIGNED` in stored
forms of their own and compares words by their bytes, so a joined `SUM(u.balance)`,
`MAX(p.user_id)` over an unsigned id or `MAX(u.name)` is refused. `AVG` and the rest still take
a bare column over a join. A statement reading one table is the exception, which is how TypeORM's query builder
(`SUM(user.balance)` over `FROM users user`) and Django (`SUM(users.balance)`) write every
aggregate: outside a subquery the qualifier can only name that table, so the argument is read
as the bare column, and measured on 8.4.11 the answer and its shape are the bare column's, the
result column still named after the call as written. The same holds for the column a JSON
reading in the projection or the `WHERE` reads — TypeORM's
`JSON_UNQUOTE(JSON_EXTRACT(user.profile, '$.city')) = 'Tokyo'`. Both look the name up among
the table's own columns afterwards, so a name that is no column of it is still refused; a
`HAVING` and an `ORDER BY` are left as written, the engine reading a bare name there as one of
the projection's aliases when no column has it.

A count of a written whole number — `COUNT(1)`, `COUNT(0)`, which TypeORM's `count` writes —
counts every row: measured on 8.4.11, it answers what `COUNT(*)` answers, value and shape, and
is named as written. `COUNT(NULL)`, which counts nothing, a word and a fraction are refused.

A reading of the moment is shifted by an interval, which is how a suite asks for the rows of
the last month: `WHERE created_at > DATE_SUB(NOW(), INTERVAL 30 DAY)`. `DATE_ADD` and
`DATE_SUB` took a column as the thing to shift, because the answer's shape was worked out
from that column's type; a reading carries its own kind, so the answer is known without
reading any column. Measured on 8.4.11: shifting `NOW()` answers a `DATETIME` of 19 whatever
the interval named, shifting `CURDATE()` by whole days, months or years answers a `DATE` of
10 and by an interval carrying a time answers a `DATETIME`, and every one of them is nullable
where the reading itself is not.

The same shift is written as a value, so a row can record a moment that is not this one. What
it meets on the other side of a comparison is the column whose form it answers: a shifted day
meets a `DATE` and a shifted moment a `DATETIME` or `TIMESTAMP`, for the reason a written one
does. `CURTIME()` is not shifted: a `TIME` holds a span rather than a moment, and shifting a
span by a month names nothing.

`CAST(col AS <type>)` is read for the four targets the engine answers exactly what MySQL
answers. Writing a column out with `CHAR` answers a `VAR_STRING` as wide as the column's own
display width counted in the four bytes utf8mb4 reserves for a character — measured on
8.4.11, an `INT` of 11 answers 44, a `BIGINT` of 20 answers 80, a `DATETIME` of 19 answers 76
and a `DATE` of 10 answers 40. `SIGNED` answers a `LONGLONG` of 21, and it **rounds**: MySQL
answers 2 for `CAST(1.5 AS SIGNED)` and -2 for `CAST(-1.5 AS SIGNED)` where the engine's own
cast cuts the fraction off, so the value is rounded before it is cast. `DATE` answers the day
out of a moment and `DATETIME` the moment a day begins.

The rest are refused, each for a measured reason. `UNSIGNED` wraps a negative into an
unsigned 64-bit number — `CAST(-3 AS UNSIGNED)` is 18446744073709551613 there — and the
engine holds an integer as an `i64`. `DECIMAL` casts and writing a `DECIMAL`
column with `CHAR` need result precision, scale and text conversion rules
separate from exact column storage. A `DOUBLE` prints by a rule of its own.
`CAST(decimal_column AS SIGNED)` is also refused: using the engine's generic
rounding would lose exact decimal digits. This limit keeps the result correct
until an exact conversion is implemented.
`CHAR(n)` cuts the value short and warns,
and so does reading a number or a day out of a word — `CAST('  7 apples' AS SIGNED)` is 7
there with a warning this does not raise.

`CONVERT(col, <type>)` means what `CAST(col AS <type>)` means and is written out the same
way, and `CONVERT(col USING utf8mb4)` means `CAST(col AS CHAR)`: measured on 8.4.11, both
write the column out in the one character set this server speaks, a `VARCHAR(200)`
reporting 800 and an `INT` 44. The other spellings are refused: another character set would
change the collation the answer carries, and the T-SQL `CONVERT(<type>, col)` and
`TRY_CONVERT` write the two the other way round and answer NULL where MySQL raises. An
unaliased cast or `CONVERT` is named after the text it was written as, as MySQL names it.

A value written out in full in the statement's own result is worked out before the
statement runs, into the text MySQL answers and the column MySQL reports, because the engine
answers a different one. Measured on 8.4.11:

- A word in quotes is a `VAR_STRING` four bytes to each character, never null — `'abc'`
  reports 12, `''` 0 and `'é'` 4 — in both protocols, where it used to report 4096 and be
  nullable. It is named after the word as it reads, without its quotes and with its
  escapes worked out: `'it''s'` is named `it's` and `'a\nb'` carries its line break.
  The spaces and control characters before it are left out of the name, `' '` being
  named nothing at all; the name stops at a NUL, writes each character outside the Basic
  Multilingual Plane as `?` (MySQL keeps a name in utf8mb3), and is cut to 255 bytes
  without splitting a character, while the column counts every character. Words written
  one after another, `'a' 'b'`, are refused: MySQL names the column after the first alone
  and sqlparser joins them into one. `N'abc'`, which MySQL answers with a deprecation
  warning, and `_binary'abc'` stay refused.
- A signed whole number is named as written, `-1` as `-1`, `- 1` as `- 1` and `+1` as
  `1`, where the engine names them `(-1)` and `(+1)`.
- A written word or whole number beside a grouping by a call keeps the shape it has on its
  own: MySQL does not store it in the table it groups in.
- A number written with a point is a `NEWDECIMAL` that reports its digits, a point and a
  sign — `1.5` reports 4 with one decimal, `.5` 3, `0.10` 5 with two, and `100.` 4 with
  none — and it answers the digits it was written with, `0.10` rather than the engine's
  `0.1`. A number with redundant leading zeroes, `007.5`, is refused: MySQL counts their
  width by a rule of its own, `007.5` three digits and `000.5` two.
- A number written with an exponent is a `DOUBLE` as wide as it was written — `1e3`
  reports 3 and `1.5e-3` 6 — and 23 when negative.
- `0x41`, `X'41'` and `b'101'` are binary strings of their bytes, a `VAR_STRING` in the
  binary character set as long as the bytes; the two hexadecimal spellings carry the
  unsigned flag and the bit spelling does not. An odd count of hexadecimal digits is
  refused, because sqlparser gives `X'4'`, a syntax error in MySQL, and `0x4`, one byte,
  the same shape.
- `CAST(<number> AS DECIMAL(p,s))` rounds half away from zero to `s` places and reports
  `p` digits, a sign and a point — `DECIMAL(10,2)` reports 12 — with `DECIMAL` alone meaning
  `DECIMAL(10,0)`. A number too wide for the type is refused: MySQL holds it to the widest
  one the type takes and warns. A word and a `DOUBLE` read into a `DECIMAL` are refused.
- `CAST('<day>' AS DATE)` and `CAST('<moment>' AS DATETIME)` answer the day or moment a
  column of that type would store, a `DATE` of 10 or a `DATETIME` of 19, nullable. A word
  naming no day is refused: MySQL answers NULL with warning 1292.
- `CAST('<document>' AS JSON)` answers the document the way MySQL stores one — keys sorted,
  the last of a repeated key kept — as a `JSON` column. A word that is no document is
  refused, which MySQL answers 3141 for.
- `CAST('<word>' AS CHAR)` and `CONVERT('<word>' USING utf8mb4)` answer the word as a
  `VAR_STRING` four bytes to each character, nullable.
- A few calls over written values alone: `HEX(n)` writes a whole number's 64
  bits, `HEX(-1)` being `FFFFFFFFFFFFFFFF`, as a `VAR_STRING` of 64, and
  `HEX('word')` its bytes, eight times its characters wide; `BIN` and `OCT`
  write the same bits as one of 260; `CHAR(65, 66)` answers the bytes of each
  number as a binary string reserving four bytes to the number, nullable with
  the binary flag; `ASCII` and `ORD` read a word's first byte and first
  character as a NOT NULL `LONGLONG` of 3 and of 21; `FIELD` finds an ASCII word
  among the ones after it without regard to case as one of 3, and `ELT` reads
  one out by its place, NULL past the last; `TRUNCATE` cuts a number with a
  point to the places asked for held to the ones written — `TRUNCATE(1.567, 2)`
  is a `NEWDECIMAL` 1.56 of 5 — and a whole number left of the point, a
  `LONGLONG` of 21. `FIELD` over a word outside ASCII, which compares by the
  collation's own weights, and `CONV` are refused.
- The calendar calls over written values: `MAKEDATE(2026, 32)` is 2026-02-01
  and `FROM_DAYS(739000)` 2023-04-25, each a `DATE` of 10 and `FROM_DAYS` NOT
  NULL; `MAKETIME(-1, 2, 3)` is `-01:02:03`, a `TIME` of 10;
  `PERIOD_DIFF(7001, 6912)` is -1199, a NOT NULL `LONGLONG` of 21; and
  `GET_FORMAT(DATE, 'ISO')` is `%Y-%m-%d`, a `VAR_STRING` of 68. A two-digit
  year is read in this century under seventy and in the last from there. A
  value MySQL answers NULL or the zero day for — `MAKEDATE(2026, 0)`,
  `FROM_DAYS(365)`, `MAKETIME(12, 60, 0)` — or refuses with 1210, as it does
  `PERIOD_DIFF(0, 0)`, is refused. Over a column none of them is taken.

Each of these, and each cast of `NULL` to the same types, is taken only where it stands in
the statement's own result. In a subquery, a derived table or a branch of a `UNION` it is
refused, because nothing there reports the shape it was worked out to.

A call stands where a column stands on the left of a comparison:
`WHERE LOWER(email) = 'a@x'`, `WHERE CHAR_LENGTH(name) > 3`, `WHERE YEAR(d) = 2024`,
`WHERE CAST(dt AS DATE) = '2024-06-15'`. A column says what it holds through its declared
type; a call says it through what it is, so the value it meets is held to that instead.

A call answering a word is compared without regard to case, which is what MySQL's collation
does after the call has answered — measured on 8.4.11, `LOWER(name) = 'ADA'` finds the row
holding `Ada`, and `LOWER(name) > 'b'` finds `bob` and `CARL`. A call answering a number
meets a number. A call answering a day or a moment is held to the form one is stored in, the
way a `DATE` or `DATETIME` column is, so `CAST(dt AS DATE) = '2024-6-15'` is refused for the
reason the same value against a column is. A `?` meets none of them: it carries no type until
it binds, and nothing puts it into that form.

Which calls can stand there is decided by what they answer, not by what they are called. The
ones that answer a real number are left out: what a `DOUBLE` compares equal to is a rule of
its own and it has not been measured. A column on either side of the operator is read the way
it always was, so `WHERE d = CURDATE()` is unchanged.

A call meets another call answering the same kind — `LOWER(name) = UPPER(email)`,
`CHAR_LENGTH(name) < CHAR_LENGTH(nick)`, `DATE(dt) = DATE(d)`, `YEAR(dt) = YEAR(d)` — and a
column holding that kind — `email = LOWER(name)`, `CHAR_LENGTH(name) = id`, `DATE(dt) = d`.
Two words compare without regard to case under `utf8mb4_0900_ai_ci`, which is what MySQL
compares them under when every column either side reads carries it; a column under
`utf8mb4_bin` or `utf8mb4_unicode_ci` is refused there, as it is under a call against a
written word (measured on 8.4.11, `LOWER(name) = b` with `b` under `utf8mb4_bin` compares
under `utf8mb4_bin`). A word against a number, a day against a moment, and a `DATETIME(3)`
against a call answering a moment are refused. `LOWER('ANN')` and `UPPER('ann')` over a word
written in ASCII are read as the word they answer, so `LOWER(name) = LOWER('ANN')` is
`LOWER(name) = 'ann'`; outside ASCII MySQL changes case by Unicode rules and the call is
refused. Each of these, and a call against a written value, is taken in an `UPDATE` and a
`DELETE` as well, where a call against a written value used to be refused.

One `+`, `-` or `*` over a column — `WHERE age + 1 > 10`, `WHERE age - n = 1`,
`WHERE age * n > 50` — is compared with a number written out, in a `SELECT`, an `UPDATE` and a
`DELETE`. Each column it reads has to be a signed whole number no wider than an `INT`, and a
written operand no wider than one either, so the answer stays inside a `BIGINT` where the two
engines agree: measured on 8.4.11, `big + 1` past the largest `BIGINT` is 1690 and an
`INT UNSIGNED` difference below zero is 1690 too, where the engine would go on in floating point.
A `DOUBLE`, a `DECIMAL` or a word read through arithmetic, a word or a `?` on the other side,
`/`, and more than one operator are refused.

`COALESCE(col, value)` and `IFNULL(col, value)` on one side of a comparison — `WHERE
COALESCE(age, 0) > 10`, `WHERE IFNULL(name, '') = ''` — are taken over a whole-number column
other than a `BIGINT UNSIGNED`, a `DOUBLE`, and a `VARCHAR` or `TEXT`, with the fallback and the
value compared with each held to the column as a comparison against the column would hold them.
Words compare under `utf8mb4_0900_ai_ci`, which the column has to carry: measured,
`COALESCE(b, '') = 'dan'` over a `utf8mb4_bin` `b` compares under `utf8mb4_bin`. Measured and
matched: `COALESCE(age, 0) > 1.5` finds the rows above one and a half, and
`IFNULL(name, 'zzz') > 'b'` finds the row holding NULL. A fallback on a `DECIMAL`, a day or a
moment, a word against a number either way, and more than two arguments are refused.

A `JSON` column in a checked one-table `SELECT` accepts `=`, `<>`, `<=>`, `<`,
`<=`, `>` and `>=` against written strings and signed 64-bit integers, plus
`IN` and `NOT IN` against those values and SQL `NULL`. A written
string is a JSON *string*, compared byte for byte with the stored value; it is
not parsed as a document. Measured on 8.4.11 against a row holding
`{"a": 1, "b": 2}`, `doc = '{"a": 1, "b": 2}'` finds **nothing**, while
`doc = 'word'` finds a row holding `"word"` and `doc = '"word"'` does not.
A written integer meets JSON integers and JSON doubles, using the JSON double's
canonical decimal text for exact comparison against the integer:
`9007199254740992` and `9007199254740993` stay distinct.
The JSON boolean `true` is not the SQL number 1. SQL `NULL` still makes `=`
and `<>` unknown; `<=> NULL` matches SQL NULL, not the JSON value `null`.

An ordering comparison follows the JSON type precedence measured on 8.4.11:
JSON null, number, string, object, array, boolean. Within strings it compares
the decoded bytes; within numbers it preserves the integer boundary at 2^53.
A bare or qualified `ORDER BY` on the JSON column, a fractional or out-of-`i64`
numeric right side, a bound `?`, and an explicit collation are refused. MySQL
can interpret a bound JSON comparison differently depending on earlier bound
parameter types in the same prepared statement, so the current parameter value
alone cannot safely select a comparison rule. JSON comparisons across several
source tables, through
a view, or in checked DML are also refused because those paths have no typed
JSON rendering. Measured on 8.4.11, `doc > '[1, 1]'` follows JSON's type
precedence, which comparing the stored text would not. JSON grouping and
other ordering forms need a separate audit.

A `BLOB` column is not compared yet, and that one is a gap. Measured: MySQL compares the
bytes, so `payload = 'ABC'` finds no row holding `abc` and `payload > 'a'` reads them in byte
order — which is the engine's own comparison, without the collation a text column asks for.
Taking it needs the renderer to be told a column is binary so it leaves that collation off,
which is the same channel that tells it a column is text.

A column compared with another column — `WHERE name = email`, `WHERE age > score`,
`WHERE p.n > r.n` in a comma join, `JOIN r ON p.n > r.n` — is taken in a `SELECT`, an
`UPDATE` and a `DELETE`, text or prepared, when the two are a pair MySQL and the engine
compare alike as they are stored: two whole numbers of any width, signed or unsigned; a
`DOUBLE` with another `DOUBLE` or with a whole number no wider than an `INT`; two `DECIMAL`s
of any sizes, which both compare exactly; two `DATE`s,
two `YEAR`s; two `DATETIME`s or two `TIMESTAMP`s of one fractional precision; two `TIME`s of
one precision compared for sameness; and two `VARCHAR` or `TEXT` columns under one collation,
which the engine reads off the left column as MySQL reads it off both. Measured on 8.4.11 and
matched: `name = email` folds case and accents under `utf8mb4_0900_ai_ci` (`'Dan'` is `'dan'`,
`'é'` is `'E'`) and does not pad (`'a '` is not `'a'`), two `utf8mb4_bin` columns pad and keep
case, two `utf8mb4_unicode_ci` columns pad and fold, a `BIGINT` of 9223372036854775807 is less
than a `BIGINT UNSIGNED` of 9223372036854775808. Every other pair is refused, each because
MySQL converts one side to the other's type first: measured, `utf8mb4_0900_ai_ci` against
`utf8mb4_unicode_ci` is 1267 and either against `utf8mb4_bin` compares under `utf8mb4_bin`; a
`CHAR` loses its trailing spaces; an `INT` of 2 equals the word `'2abc'`; a `BIGINT` of
9007199254740993 equals a `DOUBLE` of 9007199254740992, both read as doubles; a `DATETIME`
equals a `DATETIME(3)` holding the same moment, a `DATETIME` holding midnight equals the
`DATE` of that day, and a `TIMESTAMP` equals the `DATETIME` of the same moment. A `DECIMAL`
with a whole number is refused because the engine compares that pair as two doubles: measured,
a `DECIMAL(30,20)` of 1.00000000000000000001 equals an `INT` of 1 here and not in MySQL. A pair
under a written `COLLATE`, a pair over a view, and `FLOAT`, `JSON`, `BLOB`, `ENUM`, `SET` and
`BIT` columns are refused too. A comma join's `a.x = b.y` and a `SELECT` join's
`ON a.x = b.y` used to be taken without looking at the two types, and are held to the same rule
now; the `ON` of a joined `UPDATE` or `DELETE` still is not.

`DATE(col)` reads the day out of a moment, which is the other spelling of
`CAST(col AS DATE)`. Measured on 8.4.11, both answer a nullable `DATE` of length 10 in the
binary character set, so they are one thing here and the shorter spelling stands on the left
of a comparison the way the longer one does: `WHERE DATE(created_at) = '2024-06-15'` finds
the rows of that day. `TIME(col)` is not read — a `TIME` holds a span running past a day and
the engine's reader answers NULL for one, so the two would not agree.

`(a, b) IN ((1, 'x'), (2, 'y'))` looks a key of more than one column up, which is how an
ORM asks for a set of rows by their composite key. The engine has no list of rows to ask that
of, so it is asked the question the row list means: each row is its columns compared one by
one and joined by `AND`, and the rows are joined by `OR`. Each of those comparisons is the
one that would have been written out, so every column is held to its own type and a word is
read under the collation — measured on 8.4.11, `(a, b) IN ((1, 'X'))` finds the row holding
`x`. Three-valued logic comes along with it: a row holding NULL in one of the columns is left
out of the `NOT IN` as well as the `IN`, which is what `NOT (NULL AND true)` answers and what
MySQL answers.

`ORDER BY n IS NULL, n` sends the rows holding nothing last, which is how a statement asks
for that: neither MySQL nor the engine has a word for it, and both answer the test as 0 or 1
and sort by that. Measured on 8.4.11, the two order the rows the same way, with the flag
written either way round and in either direction. Left off, the rows holding nothing come
first, which is where both put them. The test has to be over a column rather than over
something that reduces to one, which is the rule every ordering term here follows.

The NULL-safe equality operator `<=>` is translated to the engine's `IS`
operator. Like `=`, text column comparisons with `<=>` receive `COLLATE MYSQL_UCA9_AI_CI`
so MySQL's case-insensitivity is preserved, while integer and NULL operands
evaluate without coercion. Both `WHERE col <=> 1` and `WHERE col <=> NULL`
(as well as prepared parameters) evaluate identically to MySQL.

`LIKE` on a text column uses a dedicated matcher over the frozen UCA9
primary weights. Each literal character and `_` consumes one Unicode scalar;
`%` consumes zero or more. This distinction matters: `é LIKE 'e'` is true,
but `ß LIKE 'ss'` and `e` followed by a combining acute accent `LIKE 'é'`
are false, even though those full strings compare equal under the collation.
The matcher preserves MySQL's default backslash escape, an explicit `ESCAPE`,
and the absence of an implicit escape under `NO_BACKSLASH_ESCAPES`. Written
and bound patterns use the same matcher. Oversized or overly expensive
patterns fail closed.

`REGEXP` has different rules from collation equality: MySQL uses ICU full case
folding and remains accent-sensitive. The dialect accepts its checked ASCII
forms. It refuses a non-ASCII subject or pattern rather than return a wrong
row, because Rust regex does not reproduce ICU expansions such as
`ß REGEXP 'ss'`. A `LIKE` over an explicit `utf8mb4_bin` column is refused
until a matching PAD SPACE pattern matcher is available; `LIKE` over a view is
also refused when its source column collation cannot be checked.

The scalar calls taken so far are `LOWER`, `UPPER`, `REVERSE`, `REPEAT`,
`REPLACE`, `LPAD`, `RPAD`, `INSTR`, `LOCATE` (2 arguments), `HEX` (text columns),
`LENGTH` (and its `OCTET_LENGTH` spelling), `CHAR_LENGTH` (and its
`CHARACTER_LENGTH` spelling), `NOW()` with
`CURRENT_TIMESTAMP`, `ABS`, `SIGN`, `SQRT`, `POW` (with `POWER`), `MOD`, `ROUND`,
`GREATEST`, `LEAST`, `NULLIF`,
`IFNULL` with `COALESCE`, `CONCAT`, `CONCAT_WS`, `SUBSTRING` (with `SUBSTR`),
`SUBSTRING_INDEX`, `MD5`, `SHA1` (with `SHA`), `SHA2`, and `LEFT` with `RIGHT`. A `CASE` and its call spelling `IF` are taken
alongside them. Each answers a shape measured on 8.4.11.

Over a `VARCHAR(8)`, which reports length 32: `LOWER`, `UPPER`, `REVERSE`,
`REPLACE` and every `TRIM` form answer a `VAR_STRING` of that same 32 with the
not-fixed decimals value; `REPEAT(v, 3)` reports 96; `LPAD(v, 6, '*')` and
`RPAD(v, 6, '*')` report 24; `HEX(v)` reports a `VAR_STRING` of length 64 with
`latin1_swedish_ci` (8) collation (numeric columns are refused); `INSTR` and
`LOCATE` (with arguments swapped to match the engine's `instr`) answer a
`LONGLONG` of length 11, decimals 0, and `BINARY | NUM` flags, following the
engine's case-sensitivity; and `LENGTH` and `CHAR_LENGTH` a `LONGLONG` of length
10. Over an `INT` of length 11 and a
`DECIMAL(10,2)` of length 12: `ABS` keeps the column's own width and scale,
`MOD` keeps the column's numeric shape (widening `INT` to `LONGLONG` 11) and answers `BINARY NUM` (not NOT NULL since division by zero yields NULL),
`SIGN` answers a `LONGLONG` of length 21 however wide the argument was, and
`ROUND`, `FLOOR`, `CEIL` and `CEILING` answer the kind of number their column
holds, as the next paragraphs say (each carries `NOT NULL` when its column does),
`SQRT` and `POW` answer a `DOUBLE` of length 23 and not-fixed decimals (31),
`GREATEST` and `LEAST` take 2 or more homogenous arguments (all integers or all text) and answer the widest width (e.g. `LONGLONG` 11 for integer, or `max_len * 4` for text), preserving `NOT NULL` only if all arguments are non-null;
`NULLIF(expr1, expr2)` answers expr1's shape (widening `INT` to `LONGLONG` 11) with `NOT_NULL_FLAG` cleared since matching arguments yield NULL;
and `IFNULL` keeps the width.
`NOW()` answers a `DATETIME` of length 19. `CONCAT` is as wide as its arguments
laid end to end, a string literal counting the characters it spells, so
`CONCAT(v, 'z')` over that `VARCHAR(8)` reports 36 and `CONCAT(v, v)` 64; `LEFT`
and `RIGHT` are as wide as the count they were asked for, so `LEFT(v, 2)`
reports 8. `LENGTH`, `CHAR_LENGTH`, `INSTR`, `LOCATE` and `TRUNCATE` carry
`NOT NULL` over a NOT NULL column, measured on 8.4.11 (they used to leave it
off), and nothing a call answers over a column read through the outer side of
a join does.

Over a `TEXT` the answer outgrows a `VAR_STRING`. Measured on 8.4.11: `LOWER`,
`UPPER`, `REVERSE`, `REPLACE` and `TRIM` over one answer a `MEDIUM_BLOB` of
1048560 with no flags — they used to report the `TEXT`'s own `VAR_STRING` of
262140 — and a `CONCAT` or `CONCAT_WS` naming one answers a `MEDIUM_BLOB` in
which every part counts four times over: the `TEXT`'s 262140 bytes, and each
word and column beside it, so `CONCAT(t)` reports 1048560, `CONCAT('x', t)`
1048576 and `CONCAT(title, ': ', body)` over a `VARCHAR(200)` 1051792. `LEFT`
over a `TEXT` stays as wide as its count. Over a `MEDIUMTEXT` or a `LONGTEXT`
the answer is a `LONG_BLOB`, which is refused. The binary protocol sends a
`MEDIUM_BLOB` as length-encoded bytes.

`POSITION(x IN col)` is `LOCATE(x, col)` written with a keyword and answers
what it answers. `ASCII`, `ORD`, `CRC32`, `QUOTE` and `TO_BASE64` read the
bytes of a value, which the engine has no calls for, so the dialect answers
each. Measured on 8.4.11: `ASCII` answers the first byte — 195 for `Ünï` — as
a `LONGLONG` of 3, `ORD` the bytes of the first character as one number —
50076 — as one of 21, and `CRC32` the zlib checksum as an unsigned one of 10,
each NOT NULL over a NOT NULL column. `QUOTE` writes the value in quotes with a
backslash before a backslash and a quote, `\0` for a NUL and `\Z` for a
Ctrl-Z, answers the word `NULL` for a NULL, and reserves two characters for
each the value can spell and two more — 88 over a `VARCHAR(10)`. `TO_BASE64`
puts a newline after every 76 characters and reserves what the base64 of the
value's bytes runs to, newlines counted — 324 over a `VARCHAR(15)`, 1732 over
a `VARCHAR(80)`. Both answer a nullable `VAR_STRING` with no flags. `CRC32`,
`QUOTE` and `TO_BASE64` write a number or a moment out first, one byte to the
character. `QUOTE` and `TO_BASE64` over a `TEXT` answer a `MEDIUM_BLOB` by a
width rule not measured, and are refused; so are `CHARSET` and `COLLATION`,
whose answer is the column's collation, which the statement does not carry.

`INET_ATON`, `INET_NTOA` and `IS_IPV4` store and check an IPv4 address as a
number, which the engine has no calls for, so the dialect answers each.
Measured on 8.4.11: `INET_ATON` reads up to four groups each at most 255,
takes the short forms — `127.1` is 2130706433, the last group landing in the
last byte — and answers NULL for a word that is no address, as an unsigned
`LONGLONG` of 21; `INET_NTOA` writes a number from 0 through 4294967295 out
and answers NULL for any other, as a `VAR_STRING` of 124; `IS_IPV4` takes
exactly four groups of one to three digits, `010.0.0.1` being one, as a
`LONGLONG` of 1, NULL only for a NULL. An address is read out of a word column
and written out of a whole-number column. A written value MySQL answers NULL
for comes with warning 1411, which is not raised here, so it is refused.
`INET6_ATON`, `INET6_NTOA` and `IS_IPV6` answer binary strings and are not
taken.

`LPAD`, `RPAD`, `LEFT`, `RIGHT` and `CONCAT` write a number or a moment out
before they pad, cut or join it, as MySQL does — `LPAD(id, 5, '0')` over a
`BIGINT UNSIGNED` is `00001` and `LPAD(n, 4, '0')` over -5 is `00-5` — for
every kind whose spelling the engine shares: whole numbers, `BIGINT UNSIGNED`,
a `DECIMAL` with no places, and the moments. A `DOUBLE` and a `DECIMAL` with
places are refused, spelled by rules of their own. `LPAD` and `RPAD` with
nothing to pad with are refused: measured, MySQL answers an empty word where
padding is needed, `LPAD('hi', 5, '')` being `''`, and the engine answers the
value unpadded.

A `CASE` is as wide as its widest branch:
`CASE WHEN n > 1 THEN 'y' ELSE 'n' END` reports 4 and is NOT NULL, and
`IF(n > 1, 'y', 'n')` reports the same, measured identically.

Two things drop the `NOT_NULL` flag from a `CASE`, both measured on 8.4.11: no
`ELSE`, because a row matching nothing answers NULL, and a `NULL` branch. The
width is the widest branch either way — `CASE WHEN n < 3 THEN 'low' END`
and `... THEN 'low' ELSE NULL END` both report 3 characters and no flag. A
`CASE` whose every branch is NULL is refused, since there is no width left to
answer with. A branch naming a column, and `CASE col WHEN`, answer by the rules
given with the numeric `CASE` above. The `WHEN` predicate itself goes through
the same checked path a `WHERE` does, so a comparison inside it is validated
against the column's type.

`ROW_NUMBER()`, `RANK()`, `DENSE_RANK()`, `NTILE(n)`, `PERCENT_RANK()`,
`CUME_DIST()`, `LAG(col [, offset [, default]])`, `LEAD(col [, offset [, default]])`,
`FIRST_VALUE(col)`, `LAST_VALUE(col)` and `NTH_VALUE(col, n)` number, rank and
shift the rows a `SELECT` answers. Both engines
spell them the same way, so only the window is rewritten: a text column is
partitioned and ordered under the case-ignoring collation MySQL's default gives
it, the same treatment an outer `ORDER BY` gets, so `'a'` and `'A'` are one
partition in both. The column is named after the call with its whole `OVER`
clause unless an alias renames it.

An aggregate goes over a window too — `SUM`, `COUNT`, `AVG`, `MIN` and `MAX`,
each over one column, and `COUNT(*)`. With no frame written MySQL runs the
aggregate from the start of the partition to the current row when the window
orders and over the whole partition when it does not, and the engine does the
same, so a running total and a partition total both cross. Measured on 8.4.11:
a windowed aggregate answers the shape its plain form answers, with two
differences — it does not carry the binary flag, and `MIN` and `MAX` widen an
`INT` to `LONGLONG` where the plain form leaves it `LONG`. So a `SUM` over an
`INT` reports a `NEWDECIMAL` of length 33 with no decimals, an `AVG` one of
length 16 with four, and a `COUNT` a NOT NULL `LONGLONG` of length 21.

Measured on 8.4.11, whatever the window is over: the four counting calls answer
a `LONGLONG` of length 21 with no decimals, carrying the NOT NULL, unsigned and
numeric flags. `PERCENT_RANK` and `CUME_DIST` answer a `DOUBLE` of length 23
with the not-fixed decimals value, NOT NULL and numeric but not binary. `LAG`,
`LEAD`, `FIRST_VALUE`, `LAST_VALUE` and `NTH_VALUE` answer their column's own
shape, widened to
`LONGLONG` where it is an integer, and are always nullable, because the row they
reach for may not be there; they carry the numeric flag and, unlike `ABS`, not
the binary one, so a `LAG` over an `INT` reports length 11 and one over a
`DECIMAL(10,2)` reports 12 with its scale.

`LAG` and `LEAD` take a written offset and a default for the row that is not
there. Measured on 8.4.11: an offset leaves the shape the plain call reports,
and 0 reads the row itself; a default widens the answer to its own width —
`LAG(n, 1, 99999999999)` over an `INT` reports 12, `LAG(s, 1, 'a much longer
default')` over a `VARCHAR(10)` 84 — and over a NOT NULL column makes the
answer NOT NULL, which `LAG(nn)` alone is not. A default of `NULL` is no
default. A default of another kind than the column's changes the type —
`LAG(n, 1, 1.5)` answers a `NEWDECIMAL`, `LAG(d, 1, '2000-01-01')` over a
`DATE` a `VAR_STRING` — so only a whole number over a whole-number column and
a word over a `VARCHAR` are taken. An offset read from the row is refused.

A frame says which rows around this one a call reads, and `ROWS` and `RANGE` are
both taken, with `CURRENT ROW`, an unbounded end, or a non-negative whole number
of rows or of the ordering column's own units at either bound. The shorthand
`ROWS <bound>` is written out as `BETWEEN <bound> AND CURRENT ROW`, which is what
it means. Measured on 8.4.11 over four rows, the engine answers the same for
every form, including where the ordering column ties: `RANGE` takes a row's
peers in with it and `ROWS` does not. What is refused is `GROUPS`, which MySQL
answers 1235 for, and a bound that is not a plain non-negative number.

Leaving the frame out is what makes `LAST_VALUE` answer the current row rather
than the last of the partition while the window orders, as it does in MySQL.

A `WINDOW win AS (...)` clause names a window the calls then reach for by name,
which is how a statement writes one window for several calls. Each `OVER win` is
written out as the window the name stands for, which is what it means, so every
check below reads one shape; the column keeps the name MySQL gives it, `OVER
win` and all. Measured on 8.4.11, the answers are the ones the same window
written out gives. A name standing for another name, and a window built on top
of a named one — `w AS (base ORDER BY ...)` — are refused, each a second
spelling of the same thing, and so is an `OVER` naming a window nothing defined.

A window term still has to be a plain column: an expression is not something the
checked ordering path can answer for.
`NTILE(0)` and `NTH_VALUE(col, 0)` are refused, which MySQL answers 1210 for,
and so is a `LAG` or `LEAD` whose default is of another kind than its column.

A `DOUBLE` is written the way MySQL writes one, which the engine's own text form
did not do. Both write the shortest digits that read back as the same double,
and what differed was the rest: measured on 8.4.11, MySQL writes `1` for a whole
number where the engine wrote `1.0`, and `0.3333333333333333` at sixteen digits
where the engine wrote fifteen — so a value read back was not always the value
stored. It writes a number out in full while the point falls within fifteen
digits either side and writes an exponent beyond that, with no sign or padding
on it: `100000000000000` for 1e14 and `1e15` for the next one up,
`0.000000000000001` for 1e-15 and `1e-16` for the next one down, and
`123456789012345.6` written out in full at sixteen digits, so the switch is on
where the point falls rather than on how many digits there are. A negative zero
reads back as `0`. `FLOAT` and `DECIMAL` keep the renderings of their own they
already had.

Plain source columns in a windowed statement retain their table name and
nullability in protocol metadata. Computed projections are still reported
conservatively. MySQL does not report primary-key flags for the plain columns
of a windowed result either. Executing a prepared statement answers the same
columns preparing it announced, for a window and for a `UNION` alike.

`TRIM` is written as the engine's three names — `trim`, `ltrim` and `rtrim` —
because MySQL says with a side word what the engine says with a name. What to
trim has to be one character: MySQL removes whole copies of what it was given
where the engine removes any of the characters in it, and the two agree only at
one. Measured on 8.4.11, `TRIM(LEADING 'ax' FROM 'xaxabxa')` answers the string
unchanged, where the engine would strip the leading `xaxa`. `TRIM(v)` and
`TRIM([BOTH | LEADING | TRAILING] 'x' FROM v)` are the forms taken; MySQL's bare
`TRIM(LEADING FROM v)` is not, because the parser library does not read it.

A scalar subquery stands in a projection: `SELECT id, (SELECT MAX(n) FROM
inner_t) FROM outer_t`. It goes through the same reader a subquery in a `WHERE`
goes through, so the table it reads is named, authorized and checked against the
internal catalog like any other, and the inner statement is held to the rules a
bare `SELECT` is held to. Measured on 8.4.11: it answers the shape its aggregate
answers on its own — a `MAX` over an `INT` a `LONG` of 11, a `SUM` over a
`DECIMAL(10,2)` a `NEWDECIMAL` of 34 with its scale, a `COUNT` a `LONGLONG` of
21 — and is nullable whatever that aggregate is, where a plain `COUNT` is NOT
NULL. Unaliased, the column is named after the subquery's own text, parentheses
included.

Only an aggregate is taken inside one, and the reason is a difference rather
than a gap. An aggregate always answers exactly one row; a subquery answering a
column may answer several, and there the two engines part — measured on 8.4.11,
MySQL answers 1242 for a subquery that returns more than one row, where the
engine answers the first row it finds and says nothing. One row and no rows they
agree on, the second answering NULL. Taking the column form would mean answering
a number where MySQL refuses to, which is the one thing worth refusing a whole
form over.

Two subqueries over the same table record it once, so a column name inside them
does not look ambiguous where it is not.

A scalar subquery may name the statement's table from inside it, which is how
Laravel's `withCount`, `withSum` and `withMax` count or total each row's
relation: `select users.*, (select count(*) from posts where users.id =
posts.user_id) as posts_count from users order by id asc`. The aggregate may
name its column with the subquery's own table, `sum(posts.views)`, which reads
the bare column. Measured on MySQL 8.4.11, every column read straight from a
table such a subquery names reports no `NOT_NULL` flag, its key flags staying,
as on the outer side of a `LEFT JOIN`; a table it does not name keeps its flags,
and so does one only an `EXISTS` names or a subquery naming nothing outside
itself. A call or arithmetic over such a column keeps the shape it has
otherwise. A name inside the subquery written without its table is the
subquery's own column when its table has one; one its table has not got would
be the statement's, and is refused. Such a statement ordering by a bare column
is read knowing its tables' columns, the subquery's included, when every name
the two tables share is of one kind in both.

`NOW()` and `IFNULL` are NOT NULL; the rest answer NULL where their column does.
The answer belongs to no table, as MySQL reports it, and the column is named
after the call as written.

Four spellings differ underneath. MySQL's `LENGTH` counts bytes and its
`CHAR_LENGTH` counts characters, which the engine spells `octet_length` and
`length` — the other way round, and silent on ASCII if confused. The engine's
`ROUND`, `floor` and `ceil` answer a float where MySQL answers a whole number, so
the rendered SQL casts them; a float where a column promised an integer reads as
an overflow here. The engine's own `concat` skips a NULL argument where MySQL
answers NULL for the whole call, so `CONCAT` is rendered with `||`, the operator
that agrees. And `NOW()` reads the clock in UTC, which is the zone this server runs
in — see the `TIMESTAMP` note below.

Each takes one plain column, and `IFNULL` a second argument that cannot itself
be null, which is the whole reason a client writes it. A whole number falling
back onto a `DOUBLE` or a `FLOAT` answers the column's kind at a length of 23,
the fallback row included, as measured on MySQL 8.4.11. MySQL takes a text call
over a number and a numeric one over text by coercing it, which has not been
measured, so each is answered only over the kind it is for, and an expression
argument has no length this could work out.

`TRIM` is not in that list for one reason worth writing down: sqlparser gives
it its own AST shape rather than a call — MySQL's `TRIM` has `LEADING`,
`TRAILING` and `BOTH` forms — so it needs its own reading rather than another
name in a list.

`COUNT` is the one aggregate whose answer does not depend on what it counts.
Measured on MySQL 8.4.11: `COUNT(*)`, `COUNT(col)`, and `COUNT(DISTINCT col)`
all give a non-null `LONGLONG` of length 21 with the binary
collation and no decimals, and 0 rather than NULL on an empty table, while
`COUNT(col)` skips NULLs — which is what the engine does too, so nothing about
the value has to be arranged. For text columns, `COUNT(DISTINCT col)` adds
`COLLATE MYSQL_UCA9_AI_CI` so MySQL's accent- and case-insensitive comparison is respected (e.g.
`'b'` and `'B'` count as one distinct value). The column is named after the call
as written, case kept and the argument unquoted, and an alias replaces that name.
GORM counts distinct values as `COUNT(DISTINCT(user_id))`, with the column in
parentheses; measured on 8.4.11, that counts what `COUNT(DISTINCT user_id)`
counts, in the same shape, named `COUNT(DISTINCT(user_id))` as written, and so
does it here — the parentheses around a counted column, qualified or not and
with or without `DISTINCT`, are read as the bare column.

`MIN`, `MAX`, `SUM`, `AVG` and `GROUP_CONCAT` are taken too, and they needed one thing `COUNT`
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
`DOUBLE` both answer `DOUBLE` with length 23 and 31 decimals. Over an unsigned whole
number, measured on 8.4.11, `MIN` and `MAX` answer the column's own type and carry its
unsigned flag — `MAX(id)` over a `BIGINT UNSIGNED` a `LONGLONG` of 20, unsigned, which is
what a client decoding the binary protocol reads the value by — and `SUM` and `AVG` count
its digits the way they count a signed one's, 3 for a `TINYINT UNSIGNED`, 10 for an `INT
UNSIGNED` and 20 for a `BIGINT UNSIGNED`, so `SUM` over the last answers 43, and answer a
signed decimal. `GROUP_CONCAT` skips NULL values, joins the rest with a comma
unless a `SEPARATOR` says otherwise, and answers a column sized by the
session's `group_concat_max_len`, flags 0 and 31 decimals: under the default
1024 a `LONG_BLOB` of 65536 (see "`GROUP_CONCAT` and `group_concat_max_len`").

Three things hold for all four. The result is nullable whatever the column is,
because an empty table gives NULL. It belongs to no table, so the schema, table
and original-name fields are empty. And a numeric one carries the binary flag
where the plain column does not, which is measured — the aggregate's answer has
the binary collation. A `MIN` or `MAX` over a text or temporal column reports no
flags at all, losing even the binary flag a temporal column carries; also
measured.

The call has to name one plain column. An expression argument, `DISTINCT`, a
window, a filter and a qualified name are all refused, because none of them has
a type this can work out. A `SUM` or `AVG` over a text or temporal column is
refused too: MySQL answers those by coercing the column, which has not been
measured.

`ROUND(SUM(col), n)` and `ROUND(AVG(col), n)` are taken over a signed whole
number or a `DECIMAL` column, which is how a report prints a total or an
average. Measured on MySQL 8.4.11, the answer is a `NEWDECIMAL` worked out from
the aggregate's own precision and scale — a `SUM` 22 more digits than the
column and its scale, an `AVG` 4 more digits and 4 more places. Rounding to
more places than that scale, or to as many when there are any, keeps the
aggregate's shape: `ROUND(AVG(views), 6)` and `ROUND(AVG(views), 4)` over an
`INT` both answer 16 characters with 4 places and `5.0000`. Rounding to fewer
keeps the whole part, adds a digit for the carry rounding can make and keeps the
places named: `ROUND(AVG(views), 2)` answers 15 with 2, and `ROUND(SUM(views))`
34 where `SUM(views)` answers 33. It is nullable, NULL over no rows, and carries
the binary flag. MySQL takes the average as an exact decimal four places past
the column's and rounds that half away from zero, so the engine's exact
`mysql_decimal_avg` and `mysql_decimal_sum` answer it rather than its float
`AVG` — over -2 and -3, `ROUND(AVG(n))` is -3. Refused: rounding left of the
point, which the engine's decimal rounding stops short of; `MIN`, `MAX` and
`COUNT` inside, which answer shapes of their own; and a float column.

A `WITH` clause names a subquery so the statement can read it as a table.
Measured on 8.4.11: a column that comes through a CTE names the CTE as its table
and carries the base column's own type and flags — a primary key stays a primary
key — which is what this reports.

The ordinal a result column carries counts through what the CTE projected, not
through the table. A CTE can project its table's columns in any order, and
resolving straight into the table hands each column the other's metadata; the
projected names are carried for exactly that reason.

A CTE's body projects what a derived table's does and reports the same shapes —
`*`, a column under an alias, and in a body that aggregates the answers it works
out; see the derived table paragraphs.

`WITH RECURSIVE` is taken over a counted sequence — `WITH RECURSIVE n(x) AS
(SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT x FROM n` — which
is how a statement asks for a run of numbers. Every column starts at a written
whole number and steps by adding, subtracting or multiplying one; one column
steps up by a written positive number and the recursion stops at a written
`<` or `<=` bound on it; `UNION` and `UNION ALL` are both taken, and the
columns are named in a list or by the first row's aliases. Measured on MySQL
8.4.11, each column is a nullable `LONGLONG` as wide as its first value's digits
and one more — `1` and `-1` report 2 and `10` reports 3 — whatever it steps to,
with no flags, naming the sequence and itself and no database or original table.
The statement reads the sequence alone: its columns, `*`, a `COUNT(*)`, a
comparison of a column against a whole number, an `ORDER BY` and a `LIMIT`.

The engine runs a recursive CTE the way SQLite does, which answers MySQL's rows
but has no limit on how deep it goes. MySQL answers 3636 once a recursion runs
past `cte_max_recursion_depth`, 1000 by default — measured, 999 rows past the
first are answered and 1000 are not — and 1690 once a value runs past a
`BIGINT`. So only a sequence whose length and values the statement itself
decides is taken, and one that would run past either limit is refused. A
recursion whose depth depends on the rows it reads — walking a tree of parents
— is refused: the engine would loop for ever over a cycle MySQL answers 3636
for.

Refused: a recursion reading a table or read beside one; an aggregate other
than `COUNT(*)` over the sequence; and, for a CTE that does not recurse, a body
that reads more than one table or carries its own `ORDER BY` or `LIMIT`, a
column list on the name, the materialization hints, and an expression in a
body that does not aggregate. A `WHERE` comparison against a
qualified column is accepted when the qualifier matches the single source table,
alias, or CTE name, so `WHERE c.id = 1` works as expected. Multi-table joins or
unmatching qualifiers remain refused.

A `WHERE` can name a subquery: `column IN (SELECT column FROM table)`, its
`NOT IN` form, and `EXISTS`/`NOT EXISTS`. The subquery's table is read like any
other — authorized separately, and refused when it names an internal catalog
table, so it cannot hide one behind the outer query. What it does not do is name
any result column: the columns are the outer statement's, with the metadata they
would have had without the subquery, which is what MySQL reports.

A membership test raises the same coercion question a literal comparison does,
so it is held to the same rule: MySQL compares the two columns by coercing one
to the other's type and the engine compares them by affinity, so both have to be
the same kind — two signed integer columns, or two text ones. `EXISTS` records
nothing, since it compares nothing.

`IN` over a list of values — `WHERE id IN (1, 2)` — follows the same rule one
member at a time. Every member is recorded as its own checked comparison, so a
list is held to the column's type exactly as a single `=` is, and a member
outside the checked kinds is refused rather than coerced. MySQL compares each
member under the column's own collation rather than the member's, so one text
member collates the whole list: measured on MySQL 8.4.11 over rows (1,'b'),
(2,'A'), (3,'c'), `name IN ('a','C')` answers 2 and 3. NULL keeps ordinary
three-valued logic in both engines — `id IN (1, NULL)` answers 1 and
`id NOT IN (1, NULL)` answers nothing. The same rules apply to `UPDATE` and
`DELETE` predicates, where text lists collate under `MYSQL_UCA9_AI_CI` and three-valued
NULL logic applies. An empty list is refused.

A subquery may name the outer statement's column — a correlated one —
because it is written with the predicate a join is written with: a column on
each side, each saying which table it came from. `SELECT id FROM a WHERE
EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id)` answers what MySQL answers, and
so do its `NOT EXISTS` and `IN` forms with a `WHERE` of their own. A
comparison against a literal inside the subquery names its own table the same
way, and that qualifier is what says which table the value's type is checked
against.

An unqualified name inside the subquery is read the way MySQL reads one: the
subquery's column when it has one, and the outer statement's when it does not.
Measured on 8.4.11, `EXISTS (SELECT 1 FROM b WHERE tag = 'z')` reads `b.tag`
and `EXISTS (SELECT 1 FROM b WHERE name = 'one')` reads `a.name`, `b` carrying
no `name` — and the value is held to the type of whichever column it found.

An `EXISTS` may join tables inside it, which is how Laravel's `whereHas` asks
through a pivot table: `EXISTS (SELECT * FROM tags INNER JOIN post_tag ON tags.id
= post_tag.tag_id WHERE posts.id = post_tag.post_id AND name = ?)`, nested in the
`EXISTS` over `posts`. Measured on MySQL 8.4.11, a bare name there is the column
of whichever joined table has it — `tags.name` — held to that column's type and
compared under its collation, a bound `'PYTHON'` finding `python`; one two joined
tables both have answers 1052, which the engine refuses too; and `NOT EXISTS` is
`whereDoesntHave`. Only an `EXISTS` in a `SELECT`'s `WHERE` joins: an `IN`, a
scalar subquery and a subquery in an `UPDATE` or `DELETE` still read one table.

Refused: a subquery projecting more than one column or reading more than one
table outside an `EXISTS`, one carrying its own `ORDER BY` or `LIMIT`, and a
subquery anywhere but a `WHERE`.

An `UPDATE` may name the rows it changes through a join —
`UPDATE a JOIN b ON a.id = b.a_id SET a.n = 0` — and the join is written the
same way, as a subquery answering the target's own rowids. Every assignment
names the table it changes, and they all have to name the same one. Refused:
an unqualified assignment target, which MySQL resolves against the joined
tables and calls ambiguous when both carry the name; a value naming another
table, which MySQL takes from whichever row the join happened to find —
measured, `SET a.n = b.m` over two matching rows takes the first; two tables
changed at once; and an `ORDER BY` or `LIMIT`, which MySQL answers 1221 for.
MySQL's comma spelling, `UPDATE a, b SET ...`, is refused where the `JOIN` one
is taken: the parser library reads no comma between an `UPDATE`'s tables.

A `DELETE` may name the rows it removes through a join, in either of MySQL's
two spellings — `DELETE a FROM a JOIN b ON ...` and `DELETE FROM a USING a JOIN
b ON ...`. The rows the join finds are the ones to delete, so the join is
written as a subquery answering the target's own rowids and the delete takes
those. That holds for every join a `SELECT` holds, the outer one included, so
the orphan delete — `DELETE a FROM a LEFT JOIN b ON a.id = b.a_id WHERE b.id IS
NULL` — answers what MySQL answers. Both tables are read, so both are
authorized. Refused: two targets at once, which MySQL deletes from together and
which each need their own statement here; a target the join does not read,
which MySQL answers 1109 for; and an `ORDER BY` or `LIMIT`, which MySQL refuses
too.

`UNION`, `UNION ALL`, `EXCEPT` and `INTERSECT` are taken over two plain
`SELECT` branches, with the outer `ORDER BY` and `LIMIT` applying to the whole
result, as they do in MySQL. Both branches are read, so both are authorized and
neither can hide an internal catalog table behind the other.

`EXCEPT` and `INTERSECT` arrived in MySQL 8.0.31 and the engine answers them the
same way: measured on 8.4.11 over (1),(2),(3) against (2),(3),(4), `EXCEPT`
answers 1 and `INTERSECT` answers 2 and 3, and the engine answers the same for
both.

The result columns belong to neither table. Measured on 8.4.11: a compound
query's column names no table, carries none of the source column's key facts,
and keeps `NOT_NULL` only when every branch is NOT NULL. Its type is worked out
from every branch, not only the first, which is the one the engine reports:

- two whole numbers answer the wider type at its own width — `TINYINT` with
  `SMALLINT` a `SHORT` of 6, `INT` with `BIGINT` a `LONGLONG` of 20 — and a
  `TINYINT(1)` counts as a `TINYINT`, reporting 4 where the column alone
  reports 1;
- two words answer a `VAR_STRING` as wide as the wider, four bytes to the
  character, and two `CHAR`s a `CHAR` as wide as the wider;
- a `TEXT` beside a `TEXT` or a `VARCHAR` answers a `BLOB` of 1048560, where
  the column alone reports 262140, and a `DOUBLE` beside a `DOUBLE` reports 23
  where the column alone reports 22;
- a `DECIMAL`, a `DATETIME` and a `DATE` beside the same type at the same size
  keep the column's shape, and a `NULL` branch changes nothing.

Every other pair is refused: a number beside a word, which MySQL answers as a
`VAR_STRING` (`INT` with `VARCHAR(10)` is 44) and the engine keeps as two kinds
that compare differently; a whole number beside a `DOUBLE` or a `DECIMAL`; a
`DATE` beside a `DATETIME`; two collations; a column beside a written value,
which MySQL answers by a rule of its own (`SELECT i ... UNION SELECT 1` over an
`INT` is a `LONGLONG` of 11); and every type whose pair has not been measured,
`FLOAT`, `MEDIUMINT` and the unsigned ones among them.

A `UNION`, `EXCEPT` or `INTERSECT` that drops repeated rows is refused over
words compared without regard to case. Measured: `'aa'` in one branch and
`'AA'` in the other answer `aa` in MySQL, which keeps the first it meets, and
`AA` in the engine. `UNION ALL` keeps both and is taken, and so is a column
declared `utf8mb4_bin`, where the two are different words.

A table holding a `DECIMAL` is read by a union that does not name the
`DECIMAL`: only the first branch's table is read for column types, so a
`DECIMAL` named in a later branch, or reached through a wildcard or an
expression, is still refused.

Refused: `EXCEPT ALL` and `INTERSECT ALL`, which keep duplicates the plain forms
collapse — measured on 8.4.11 over rows (1), (1), (2) against (2), `EXCEPT`
answers one 1 and `EXCEPT ALL` answers two, and the engine has no spelling for
the second, so it is refused rather than answered with the first's rows. A
parenthesised branch is taken — `(SELECT id FROM a) UNION (SELECT id FROM b)` —
as long as it holds a plain `SELECT`; a branch carrying its own `ORDER BY`,
`LIMIT` or `WITH` is refused.

A `JOIN` reads two or more tables, with an `ON` that equates whole
columns. That equality is what makes a join crossable at all: a column against a
column raises no coercion question, where a literal comparison still has to go
through the checked path. `AND` chains several equalities, and a chain of joins
is taken as readily as one.

Every result column says which table it came from, under the reference the
statement used — `SELECT o.name FROM owners AS o` reports table `o` and original
table `owners`, which is what MySQL reports. That needed the result metadata to
hold several tables rather than one, and an aggregate or an arithmetic operand
that names a column now looks for it across all of them, refusing a name two
tables share rather than answering from whichever came first.

A `USING (col, ...)` is the other way to write the match. Both engines merge the
named column into one result column, so the engine's own `USING` is what gets
written, and the merged name is the one unqualified name a joined projection may
carry. Measured on 8.4.11 over `a(id INT NOT NULL, x)` and `b(id INT NOT NULL,
y)`: the merged `id` is reported once, against the left table for a `JOIN` and a
`LEFT JOIN` and against the right one for a `RIGHT JOIN` — the side that keeps
every row and so cannot answer NULL for it — and keeps its `NOT_NULL` flag. The
engine reports the same table in each case. Refused: a `USING` naming no column
or a qualified one.

Two rules keep this honest. A join's projection has to name its tables outside
what a `USING` merges, because an unqualified name is ambiguous whenever both
tables carry it and every metadata lookup here is by name. And every table a
join reads is authorized
separately, so a table-level grant on one of them is not a grant on the
statement; an internal catalog table is refused wherever it appears, not just in
the first position.

`LEFT JOIN` and `RIGHT JOIN` come with it, and they change one thing about the
result metadata. Measured on 8.4.11: a `NOT NULL` column on the side that can go
missing reports no `NOT_NULL` flag, because a row with no match answers NULL for
it, while its key flags stay and the other side keeps everything. A `RIGHT JOIN`
is the mirror image, and a chain marks every table the join can leave out.
`CROSS JOIN` produces the full Cartesian product without an `ON` clause, and
preserves the `NOT_NULL` flag on both tables because neither side can go missing.

MySQL's comma join is that same cross join, and a `WHERE` naming a qualified
column on each side is what bounds it: `FROM users, accounts WHERE users.id =
accounts.user_id` answers what the written join answers. That predicate is the
one a written `ON` already takes, so it is rendered the same way and, like the
`ON`, compares text by the engine's byte order rather than the column's
collation.

An unqualified name in a joined projection is resolved the way MySQL resolves
it: the one column across the joined tables that carries the name, or 1052 when
more than one does. Measured on 8.4.11, that is `Column 'id' in field list is
ambiguous`.

A `WHERE` comparison against a literal works in a joined statement too, and the
qualifier is what makes it work: it names which of the joined tables the
column belongs to, and that is the table the value's type is checked against. A
qualifier naming no table the statement reads is refused.

Refused: a non-equality `ON`.

Every numeric result carries MySQL's `NUM` flag. Measured on 8.4.11: a plain
`INT`, `TINYINT`, `DECIMAL`, `FLOAT` and `DOUBLE` column each report it on their
own, an aggregate and an expression report it beside the binary flag, a `UNION`
column reports it, and even a bare `SELECT NULL` does. A temporal column does
not, nor does a text one. This frontend used to set the flag nowhere, which made
every numeric column it answered differ from MySQL in the one field a client
uses to tell a number from a string.

A `TEXT` column reports `BLOB`, not `VAR_STRING`. Measured on 8.4.11: both `TEXT`
and `BLOB` columns (along with their `TINY`, `MEDIUM`, and `LONG` variants)
report type `BLOB` and carry the blob flag. They differ in collation, length,
and the binary flag: the text family reports utf8mb4 collation with lengths
1020 (`TINYTEXT`), 262140 (`TEXT`), 67108860 (`MEDIUMTEXT`), and 4294967295
(`LONGTEXT`), while the blob family reports binary collation, the binary flag,
and lengths 255 (`TINYBLOB`), 65535 (`BLOB`), 16777215 (`MEDIUMBLOB`), and
4294967295 (`LONGBLOB`). A `TEXT` value crosses the binary protocol as the same
length-encoded bytes a `BLOB` does.

The engine's own inferred type name for any text value is also `TEXT`, and that
is a different question: a string literal reports `VAR_STRING`, as MySQL reports
it, so only a column *declared* `TEXT` reports `BLOB`.

Measured, a `MIN` or `MAX` over a `TEXT` column reports length 1048560, which
the protocol metadata now preserves.

`GROUP BY` is taken over whole columns and over the calls a report groups by —
`DATE`, `YEAR`, `MONTH`, `DAY`, the other readings of a moment's part,
`LAST_DAY`, `DAYNAME`, `MONTHNAME` and `DATE_FORMAT` over a column, written out
or named by the projection's alias for it (`... AS m ... GROUP BY m`) — and is
held to `ONLY_FULL_GROUP_BY`. A key may also be the place of a projected whole
column, as Django writes `GROUP BY 1`: measured on 8.4.11, it groups by that
column and answers what naming it answers; a place holding an aggregate answers
1056 and `0` or one past the last 1054, and those, a place holding a call and one
beside `WITH ROLLUP` are refused. That mode is in MySQL 8.4's default `sql_mode` and this
server takes a client's `SET sql_mode` naming it, so the rule is enforced rather
than assumed. Measured on MySQL 8.4.11:

- A projected or ordered expression answers 1055 unless it is a grouping key as
  written, or every column it names outside an aggregate is a key that is a
  whole column. `UPPER(title)` passes under `GROUP BY title`; `created_at`,
  `YEAR(created_at)` and even `UPPER(DATE(created_at))` do not pass under
  `GROUP BY DATE(created_at)` — MySQL matches a whole key, not a key inside a
  larger expression. A key written with its function name in another case, or
  its column with the table's name, is the same key.
- A `HAVING` answers 1054 for a column that is not a whole-column key, even one
  inside a key — `HAVING DATE(created_at) > ...` under `GROUP BY
  DATE(created_at)` — and even one a primary key decides. It may name the
  projection's aliases, `HAVING d > ...` included.

Each of those is refused here where MySQL answers its error. So are a wildcard
projection, whose columns cannot be checked against the grouping, and a scalar
subquery in a grouped projection naming any column — whose outer columns are not
worked out; `(SELECT COUNT(*) FROM users)` names none and is taken.

A projected column the keys decide is taken beside them, which is how Django
groups a report — `SELECT users.name, COUNT(posts.id) ... FROM users LEFT OUTER
JOIN posts ON (users.id = posts.user_id) GROUP BY users.id` — and how the mysql
client's script groups posts with their tags, `GROUP BY u.id, p.id`. Measured on
MySQL 8.4.11: keys holding a table's primary key, or a unique key whose columns
are all `NOT NULL`, decide every column of that table — `GROUP BY id` and `GROUP
BY email` pass, a unique key over a nullable column and part of a composite
primary key do not, and neither does `GROUP BY id + 0`. A join's `ON` carries a
decided column to the column it matches, alone or among others joined by `AND`:
`users u JOIN posts p ON u.id = p.user_id GROUP BY p.id` decides `u.name`. A
`LEFT JOIN` carries one only from the tables before it to the table it adds, and
only when every column of those tables its `ON` names is decided — `posts p LEFT
JOIN users u ON u.id = p.user_id GROUP BY p.id` decides `u.name`, `users u LEFT
JOIN posts p ... GROUP BY p.id` does not decide `u.name`, and `ON p.user_id =
p.id AND u.id = p.user_id` decides nothing under `GROUP BY p.user_id`, the rows
it leaves unmatched differing within a group. A table's keys decide its own
columns on either side, a `LEFT JOIN`'s missing row answering NULL for all of
them. Everything else MySQL answers 1055 for is refused here. Some forms MySQL
takes are refused too, not having been worked out: a match in the `WHERE`, a
comma join, `USING`, a `RIGHT JOIN`, a match between columns that are not both
whole numbers, a derived table or a view among the tables, a decided column in
the `ORDER BY` or the `HAVING`, and the same grouping in a subquery, a `UNION`,
a view, `INSERT ... SELECT` or `CREATE TABLE ... AS SELECT`. A call over
words the client wrote itself is not taken as a key — `GROUP BY UPPER(title)` —
because MySQL groups words under the column's collation, where `a` and `A` are
one group, and the engine groups the call's answer by its bytes; `DATE_FORMAT`
and the day and month names answer words too, but only words one format
writes, none two of which differ in case or accent alone. A key is rendered the
way the projection renders the same call, so the engine groups on the value the
client reads back.

A statement grouping by whole columns keeps each column's result metadata and
each aggregate's, exactly as without a `GROUP BY`. One grouping by a call
reports different shapes, because MySQL groups it in a temporary table and
reports each answer that table stores as the table's column — measured, over
`GROUP BY DATE(created_at)`, `COUNT(*)` loses its binary flag. A whole number is
stored as a `LONG` when it is eleven characters or fewer, so `YEAR(created_at)`
answers a `LONG` of 4 there where it answers a `YEAR` on its own, and `MONTH` a
`LONG` of 3 where it answers a `LONGLONG`. `SUM`, `MIN` and `MAX` lose the
binary flag the same way; words lose the 31 decimals a call's words carry; a day
keeps its shape; and an `AVG`, worked out afterwards from a sum and a count,
keeps its own, as does a `ROUND` over a total or an average. A grouping key carries the group flag, 32768 — the bit MySQL also
sends as the numeric flag, so a client reads a `DATE` or a `DATE_FORMAT` key as
flagged numeric. What the temporary table does to any other answer has not been
measured and is refused: a `GROUP_CONCAT`, a `MIN` over a moment, a literal,
arithmetic. MySQL uses the same table for a statement grouping by a
column no index covers, and reports the same shapes there; this keeps the
shapes MySQL reports when an index answers the grouping instead — measured,
`SELECT user_id, COUNT(*) ... GROUP BY user_id` reports the count's binary flag
over an indexed `user_id` and not over an unindexed one — because which one
MySQL takes is its planner's choice. A join grouped by whole columns is the
same: measured, Django's `GROUP BY users.id` over `users LEFT OUTER JOIN posts`
keeps the binary flag on its count and total, and SQLAlchemy's `GROUP BY
users.id, users.name`, whose key no index holds, loses it, a `MAX` there
carrying `NO_DEFAULT_VALUE` instead; this reports the first. Django's tag
counts, `GROUP BY tags.id ... ORDER BY 2 DESC`, are sorted through such a table
too, where the count loses the flag and `tags.name` its unique-key flags.

Rows come back in the order the engine groups them, sorted by key, where MySQL
answers a statement grouping in a temporary table in the order it met each
group. Neither is promised without an `ORDER BY`.

`HAVING` comes with it, over the aggregates and the grouping columns. A
comparison on a grouping column goes through the same checked path a `WHERE`
comparison does. One on an aggregate cannot, since there is no column to compare
types against, so the aggregate's own argument column is recorded instead —
`HAVING SUM(score) > 45` holds `score` to the same rule `WHERE score > 45`
would, which is what makes the integer literal safe. `COUNT` records nothing,
because it answers an integer whatever it counts. `AND`, `OR`, `NOT` and
parentheses cross; the right side has to be an exact signed integer, or — against
a `COUNT`, a `SUM`, a `MIN` or a `MAX` — a `?`, which GORM's `Having("COUNT(*) >
?", 1)` and Prisma's `groupBy` with `having: { views: { _sum: { gt: 5 } } }`
send. The aggregated column may be named with its table or the query's alias —
TypeORM writes `HAVING SUM(post.views) > 5`, Prisma `HAVING
SUM(prisma.posts.views) > ?` — and the name keeps its table, so a projection
alias of the same name does not stand in for it. A value bound against a sum,
a least or a greatest is held to what may be bound against its column: a whole
number, a word naming one, or NULL, each compared as MySQL compares it with the
total. Measured on 8.4.11, a `DOUBLE` 2.9 and the word `'2.9'` compare there as
2.9, which the engine would too, but a fraction against a whole-number column
is refused here as it is in a `WHERE`. Measured on
8.4.11, MySQL reads a value bound there as a whole number: a bound `LONGLONG`
and a word naming a whole number compare as that number, a `DOUBLE` as a number
(`COUNT(*) > 1.5` finds the groups of two, `> 0.5` every group), and NULL finds
no group. Those are taken. Any other word is refused when the statement runs:
MySQL converts it by its own rule, `'abc'` reading as 0 with warning 1292
"Truncated incorrect INTEGER value", where the engine would order a word after
every number.
A count
also takes a word naming a whole number, which Rails writes for a bound one —
`HAVING (COUNT(*) > '1')`: measured on 8.4.11, MySQL compares the two as
doubles, so `'1'` and `'02'` are the numbers they name and raise nothing,
where `'1abc'` warns 1292; a word with a point, a space or anything else
beside its digits is refused, and so is a word against any other aggregate.

A `HAVING` with no `GROUP BY` is taken, over the one implicit group of every
row. Measured on MySQL 8.4.11 over rows (1,'a',10), (2,'a',30), (3,'b',20):
`SELECT COUNT(*) FROM t HAVING COUNT(*) > 1` answers 3 and `... > 5` answers no
rows at all, and the engine answers the same for both, so the group is filtered
out whole rather than emptied. What MySQL refuses once a statement is
aggregated is a bare column, which has no single row to come from: 1140 for one
in the projection, 1054 for one in the `HAVING`. Both are refused here rather
than answered, so only aggregates and literals reach the engine.

A `HAVING` over a statement that groups nothing and aggregates nothing is the
other reading, and MySQL answers rows for it: `SELECT id FROM t HAVING id > 1`
answers 2, 3 and 4 over rows 1..4, filtering rows rather than groups. The
engine would read the same statement as one group of every row, so the test is
written where the rows are filtered — beside the `WHERE` if there is one,
joined to it with `AND` — and the answer matches. What it may name is only a
column the projection carries, which is the rule `only_full_group_by` holds it
to: measured on 8.4.11, the same statement testing an unprojected `n` answers
1054, with or without a `WHERE` beside it, and that keeps its refusal. A
column the projection carries only under an alias — `SELECT n AS m FROM t
HAVING m > 1` — is refused too, being a name that has to be resolved to the
projection first. `IS NULL` and
`IS NOT NULL` cross alongside a comparison, since both are tests the `WHERE`
already takes.

`ORDER BY` sees the same expressions. A grouped query orders by what it
selected as often as not, so an aggregate call and integer arithmetic are both
taken there, rendered the way they are in a projection. An ordinal — `ORDER BY
1` — names the nth projected column, and is resolved to that projection and
rendered as if it had been written out, so `ORDER BY 2` and `ORDER BY name`
order the same way, collation included. Only a bare positive integer is
positional, which is what MySQL does: `ORDER BY -1` and `ORDER BY 1+1` are
constant expressions that order nothing, and both stay refused here. Two forms
diverge. An ordinal over a single wildcard projection (`SELECT * FROM t ORDER BY 2`)
is resolved by fetching the table's column list from the schema catalog on a second
rendering pass, ordering by the referenced column with collation applied. Mixed wildcard
projections (`SELECT t.*, id FROM t ORDER BY 2`) remain refused because counting
through each wildcard requires knowing how many columns it expands to. An ordinal past
the projection is refused where MySQL answers 1054.

A window naming neither a partition nor an order is the whole result as one
frame, and `COUNT(*) OVER ()` is how a paged query asks for the count of it
beside each row. Measured on MySQL 8.4.11 over three rows and matched: the
count answers 3 on all three, and `SUM`, `MAX`, `MIN` and `AVG` each answer the
whole set's value the same way, in the shapes their windowed forms already
report. A `PARTITION BY` still narrows the window to the rows sharing its
value.

A ranking over the same window keeps its refusal. `ROW_NUMBER() OVER ()`
numbers the rows in whatever order they were read, and the two need not read
them alike — which is why the empty window was refused outright before, and why
it is now refused only for the calls that depend on the order.

A comparison may name a collation too, and MySQL takes it written on the column
or on the value. Measured on 8.4.11 over 'alpha', 'Alpha', 'ALPHA' and 'beta':
naming none finds all three spellings, `utf8mb4_bin` finds the one spelled
exactly so — either way round — and `utf8mb4_0900_ai_ci` finds what naming none
finds. It holds over an inequality and over an ordering comparison as well:
`<> 'alpha' COLLATE utf8mb4_bin` answers the other three rows and
`< 'alpha' COLLATE utf8mb4_bin` the two spelled with capitals, which come first
in byte order. So the byte collation drops the UCA9 collation a text comparison
otherwise asks for, and nothing else changes.

`WHERE BINARY name = 'alpha'` compares the column by its bytes, which is how
a statement asks for the row spelled exactly so. MySQL reads the `BINARY` as a
cast of the column alone, where sqlparser reads it as a cast of everything
after it, so the condition is read back out with its first column compared by
its bytes — `BINARY name = 'a' AND n > 0` is `(BINARY name) = 'a' AND n > 0`.
Measured on 8.4.11 over 'alpha', 'Alpha', 'ALPHA' and 'alpha ': it finds the
first alone, `= 'alpha '` the last alone — a binary string keeps its trailing
spaces where `utf8mb4_bin` pads them away — and `<> 'alpha'` and `< 'alpha'`
compare the bytes too. `CAST(name = 'x' AS BINARY)`, which sqlparser reads the
same way and MySQL reads as a cast of the answer, is told apart by the text
before the column and refused. A bound value, a number, a `LIKE` and a
`BINARY` on the right of a comparison are refused.

A word written with the `_utf8mb4` introducer is the word: measured on 8.4.11,
`name = _utf8mb4'ALPHA'` finds what `name = 'ALPHA'` finds, in a comparison, a
membership test, a `LIKE` and a result column alike, since the introducer names
the one character set this server speaks and leaves the word the collation a
plain one has. A `SELECT` reads it without the introducer. Any other
introducer — `_latin1`, `_binary` — changes how the word compares and is
refused.

Five shapes are refused. A collation over a `LIKE` is one: the checked LIKE matcher takes the default UCA9 rules, so naming a byte
collation there requires a separate matcher — measured, MySQL answers
the one row spelled exactly so. A collation over a membership test is another,
that being written out as comparisons whose collation has not been measured.
The rest are a collation over a number, over a bound value — which carries no
text until it binds — and one from another character set, which is 1253 there.

An ordering may name a collation. Measured on MySQL 8.4.11 over 'beta',
'Alpha', 'alpha', 'Beta', 'Zulu' and 'apple': naming none orders them without
regard to case, `utf8mb4_bin` puts every capital first — which is byte order,
the engine's own — and `utf8mb4_0900_ai_ci` orders
them the way naming none does. The explicit `utf8mb4_bin` path uses a fixed
PAD SPACE collation; the default uses frozen UCA9 weights. Backwards is the same order reversed, and a collation over a
column of numbers changes nothing, which both answer alike.

A collation from another character set is 1253 in MySQL and refused here. So is
one over something that is not a column, and one written on a comparison rather
than on an ordering — neither has been measured.

`BIN` and `OCT` write a whole number out in another radix, `FIELD` answers the
place a word holds among the ones written after it, and `ELT` reads one out by
its place. The engine has none of the four, so the dialect answers them.

Measured on MySQL 8.4.11 over 12, 0, 255 and nothing, and matched: the radix
writings answer 1100/0/11111111 and 14/0/377, and nothing for nothing. A
negative number is written by its bits rather than with a sign in front —
`OCT(-3)` answers 1777777777777777777775 — which is the reading of it as
unsigned. Both report a VAR_STRING of length 260, wide enough for the
sixty-four bits a whole number can carry whatever the column's own width was,
and neither carries a flag.

`FIELD` answers a whole number of length 3 reporting NOT NULL: a word that is
not among the choices answers 0, and so does one that is nothing at all, so
there is no answer it cannot give. The match ignores case, which is what the
collation does. `ELT` answers the word at that place as a VAR_STRING as wide as
its widest choice, and nothing when the place is below one or past the last.

The choices of both are written out, being what the answer's width is worked
out from, and `BIN` and `OCT` refuse a word: measured, MySQL reads one as the
number it names, which is 0 for a word that names none.

`PI()` answers 3.141593 — six places rather than the whole of the number,
which is what its reported six decimals say — and it is the one reading here
that reports NOT NULL. `DEGREES` and `RADIANS` turn an angle round, which is
one multiplication, and the two work it out alike: measured on MySQL 8.4.11
over 0.1, 7 and 123.456, every digit agrees, as it does for `SQRT`, whose
rounding IEEE fixes exactly.

The readings a maths library rounds for itself are refused. Measured against
the engine, `ATAN(10)` answers 1.4711276743037347 in MySQL and
1.4711276743037345 here, and `TAN(10)` 0.6483608274590866 against
0.6483608274590867. The rest of that family — `SIN`, `COS`, `ASIN`, `ACOS`,
`EXP`, `LN`, `LOG`, `LOG2`, `LOG10` — comes from the same library, so agreeing
at the points tried would not be a promise, and none of them is taken.

A double is also the one answer the conformance harness cannot pin: it reads
one back a bit narrower on the verify pass than on the record pass, so the
values above are held in the Rust test and only `PI()` is pinned to the golden.

`REGEXP` and `RLIKE` — one thing under two spellings — ask whether a pattern
matches anywhere in a column. The engine keeps its own matching in an extension
this frontend does not register, and MySQL holds the match to a collation
rather than to the pattern, so the dialect answers it.

Measured on MySQL 8.4.11 over 'Alpha', 'beta', 'cafe', 'a1b' and nothing, and
matched: anchors at either end, a character class, a repeat, two choices, any
character, and the negated form all answer the same rows. A statement matches
one pattern against every row it reads, so the pattern is compiled once and the
rows after the first find it already built.

The collation decides the case, and only the case. Measured, `'Alpha' REGEXP
'alpha'` answers 1 while `'café' REGEXP 'cafe'` answers 0 — the same collation
that ignores both when comparing ignores only case when matching. So the
case-folding flag goes in front of the pattern, where a pattern that turns it
off again still can, and nothing else is done to it.

Four shapes are refused. A pattern looking ahead or naming a group again is
read by MySQL and not by the engine's matching, so it is turned away rather
than answered differently. A pattern that does not close is 3696 in MySQL and
an error here. A bound pattern carries nothing until it binds, and what a
written one is checked for could not be checked there. And a match over a
number is a coercion — measured, `5 REGEXP '5'` answers 1 — which is the rule
every comparison here already follows.

Arithmetic touching a float answers a float, and nothing else about it
matters. Measured on MySQL 8.4.11 over a `DOUBLE` holding 1.5: `d + 1`,
`d - 1`, `d * 2`, `d / 2`, `d + e`, `d + n` over an `INT`, `d + amount` over a
`DECIMAL(10,2)` and `n + d` all answer a DOUBLE of length 23 with 31 decimals —
whichever side the float was on, whichever operator it was, and whatever the
other side carried. So a float swallows the precision and scale rules rather
than taking part in them, and an aggregate over one does the same: `SUM(d)`,
`SUM(d) + 1` and `MAX(d) * 2` each answer that shape too.

Arithmetic takes an aggregate where it takes a column, which is how a report
adjusts a total — `SUM(amount) * 2`, `COUNT(*) + 1`, `SUM(n) + SUM(m)`. It also
takes a decimal column, which it did not before.

Measured on MySQL 8.4.11 over rows (5, 2, 1.50), (3, 4, 2.25), (9, 6, 3.00),
three rules cover the lot. Adding and subtracting keep the widest whole part
and the widest scale and add a digit: `amount + 1` over a `DECIMAL(10,2)`
answers 11 digits with 2 places, and `SUM(n) + SUM(amount)` 35 with 2.
Multiplying adds both precisions and both scales: `SUM(amount) * 2` answers 33
with 2. Dividing widens the left side by four digits and four places whatever
the right side is: `AVG(n) / 2` answers 18 with 8.

Whether the answer is a decimal is not the same question as whether it carries
places. A `SUM` and an `AVG` answer a decimal whatever they were given, so
`SUM(n)` over an `INT` answers one with no places at all and `SUM(n) + 1`
answers one too — where `COUNT(*) + 1` and `MAX(n) + 1` each answer a whole
number, a count and a highest over a whole number being whole numbers
themselves. Nullability follows the operands: a count and a digit are both
never null, so their sum reports NOT NULL, and an aggregate over a column never
does — an empty table answers NULL.

`GROUP_CONCAT` and a sample deviation are refused, being no kind of number this
arithmetic reads, and so is a windowed aggregate, which is a window rather than
an aggregate. A float column is refused too, carrying no precision and scale of
its own.

Once a statement aggregates and groups nothing, every row it read has gone into
one answer, so a bare column has no single row to come from. MySQL says so with
1140, and this now says so too rather than answering rows. Measured on 8.4.11:
`SELECT id, SUM(n) FROM t` is 1140, and so are the column written after the
total, a column inside arithmetic over the total — `id + SUM(n)` — a column
inside a call beside a count — `UPPER(name), COUNT(*)` — and a column beside a
total carrying a fallback. It is a column anywhere in the projection, not only
one standing on its own. A literal crosses, having no row to come from either.

Three shapes are not aggregated and keep answering a row per row. A window is
one: measured, `SELECT id, ROW_NUMBER() OVER (ORDER BY id)` and `SELECT id,
COUNT(*) OVER ()` both answer three rows over three. A subquery is another, its
aggregate belonging to the statement inside it. And a `GROUP BY` is the third,
giving every column a group to come from. An `ORDER BY` naming a column is not
a projection, and MySQL takes one over an aggregated statement.

The moment a `DATE_FORMAT` writes out or a `STR_TO_DATE` reads need not be a
column. `DATE_FORMAT(NOW(), '%Y-%m-%d')` is how a statement asks for today
written out, and `STR_TO_DATE('2024-03-05', '%Y-%m-%d')` how it reads a day out
of a word it wrote itself; neither reads a column, and neither needs to.
Measured on MySQL 8.4.11, both report exactly what the same call over a column
reports — the shape comes from the format rather than from what was read — so
the widening changes what the calls accept and nothing about what they answer.

`NOW`, `CURRENT_TIMESTAMP`, `CURDATE` and `CURRENT_DATE` are the readings
taken, each spelled the way the engine spells it. A `STR_TO_DATE` reads text,
so it takes a word written out and not a clock reading, which is a moment
already. A call inside the call is none of the three shapes and is refused, as
is a format that is not written out — the format is what says what the answer
will be.

`TIMESTAMPDIFF(<unit>, a, b)` counts whole units from the first moment to the
second, dropping whatever is left over, towards zero either way. Measured on
MySQL 8.4.11 over sixteen pairs of `DATETIME(6)` values and matched for every
unit: two days and a half backwards is −2 days, one second short of a day is 0
days, and `10:00:00.5` to the next month's `10:00:00.4` is 2678399 seconds —
the count is taken to the microsecond before the rest is dropped, where
subtracting the engine's `unixepoch` would drop each fraction first and answer
2678400. A month is counted by the calendar: it is whole once the later moment
reaches the same day of the month and the same time of day, so January 31st to
February 29th is 0 months, and a quarter and a year are three and twelve of
those. `MICROSECOND` is counted too. The whole count is the frontend's own
reading, which the rendered SQL calls. Every unit reports a whole number of
length 21, where `DATEDIFF` reports 9.

Each moment — here and in `DATEDIFF` — is a date column, a reading of the clock
or a moment written out as a word: `DATEDIFF(NOW(), created_at)` and
`TIMESTAMPDIFF(DAY, created_at, NOW())` are how a report asks how old a row is,
and measured, each reports the shape the same count over two columns reports.
A word is read the way MySQL reads one — `DATEDIFF('2024-1-5', '2024-01-01')`
is 4. A word that names no moment is refused: MySQL answers NULL for it. So are
`CURTIME()`, a span rather than a moment, a column that holds no date, which
MySQL coerces, and a `?`, which MySQL reads by the type the client binds.

An index hint — `USE`, `FORCE` or `IGNORE INDEX`, in either the `INDEX` or the
`KEY` spelling — is dropped. It says which key to plan with and nothing about
which rows come back: measured on MySQL 8.4.11 over three rows, each of the
three answers what the statement answers without one, with two keys named at
once, with a `FOR ORDER BY` scope, under an alias and on either side of a join.

What a hint does say is that the key exists. Measured, `FORCE INDEX
(by_nothing)` answers 1176 rather than the rows, so the names travel out of the
renderer with the source and the frontend holds them to the table's own keys —
`PRIMARY` counting as one whenever the table has a primary key, which is the
name `SHOW INDEX` reports for it whatever the stored DDL called it. A key that
exists on another table is turned away with a key that exists nowhere.

A hint on the target of an `UPDATE` or a `DELETE` is still refused, that shape
not having been measured.

The calendar readings a report writes are taken: `QUARTER`, `WEEKDAY`,
`DAYOFWEEK`, `DAYOFYEAR`, `DAYOFMONTH`, `LAST_DAY`, and `EXTRACT(<field> FROM
...)`. The engine has none of them by name, so each is counted off what it does
have — the quarter off the month, the two weekday numberings off its own, and
the day a month ends on by walking to the start of the next month and back one
day.

Measured on MySQL 8.4.11 over 2024-03-05 (a Tuesday), 2024-01-01 (a Monday),
2024-12-31 and 2023-02-28, and matched: `WEEKDAY` counts the week from Monday
as 0 and `DAYOFWEEK` from Sunday as 1, `DAYOFYEAR` answers 366 in a leap year,
and `LAST_DAY` answers February's 28th in an ordinary one. The reported shapes
are pinned too: `QUARTER`, `WEEKDAY` and `DAYOFWEEK` each a whole number of
length 2, `DAYOFYEAR` one of length 4, `DAYOFMONTH` one of length 3, `LAST_DAY`
a DATE of length 10.

`EXTRACT` reads the same parts the call spellings read and reports the same
shapes but for one: `EXTRACT(YEAR FROM d)` answers a whole number of length 5
where `YEAR(d)` answers a YEAR of length 4. Only the fields with a call
spelling are taken; `EXTRACT(WEEK FROM ...)` and `EXTRACT(QUARTER FROM ...)`
are refused, MySQL counting those by rules of its own. Each reading says what
it answers, so `WHERE QUARTER(d) = 4` and `WHERE LAST_DAY(d) = '2024-03-31'`
are held to that rather than to a column's declared type.

`WEEK(d)` and `WEEK(d, mode)` number the week a day falls in, and `DAYNAME(d)`
and `MONTHNAME(d)` name its day and its month. Each reads a date column, a
reading of the clock or a moment written out as a word. `WEEK` is MySQL's own
reckoning, which `DATE_FORMAT`'s `%U`, `%u`, `%V` and `%v` already follow;
measured on 8.4.11 over thirteen days around six new years, in every
mode from 0 through 7, and matched — the first of January 2026 is week 0 in
mode 0 and week 52 in mode 2. A mode is a written number from 0 through 7:
MySQL takes any other by its last three bits, `WEEK(d, 8)` being `WEEK(d, 0)`,
and reads a word or a bound value by rules of its own, and those are refused.
The shapes are measured too: `WEEK` a LONGLONG of 3, and `DAYNAME` and
`MONTHNAME` a `VAR_STRING` of 36 in utf8mb4, all nullable. A name is compared
without regard to case — `WHERE DAYNAME(d) = 'sunday'` finds the Sundays.

`YEARWEEK(d)` and `YEARWEEK(d, mode)` write the year and the week together,
counting as `WEEK` does in the same mode with the bit set that keeps a week in
the year it mostly falls in, and `TO_DAYS(d)` counts the days from the year
zero by the calendar MySQL keeps. Each reads what `WEEK` reads. Measured on
8.4.11: `YEARWEEK('2026-01-15')` is 202602 and 202603 in mode 1,
`TO_DAYS('2026-01-15')` is 739996, and the first of January of the year one is
day 366, in week 53 of the year zero. `YEARWEEK` reports a LONGLONG of 7 and
`TO_DAYS` one of 8, both nullable over a NOT NULL column.

`TIME_TO_SEC` counts the seconds in a time and `SEC_TO_TIME` writes a count
back out as one, which the engine has no calls for, so the dialect answers
each. Measured on 8.4.11: a `TIME` counts whole — `-838:59:59` is -3020399 —
with a fraction of a second cut toward zero, a `DATETIME` counts its time of
day and a `DATE` none, as a `LONGLONG` of 10; `SEC_TO_TIME` writes `24:00:00`
for 86400 and `-00:00:01` for -1, holds a count past the widest time to
`838:59:59`, and answers a `TIME` of 10. Both are nullable over a NOT NULL
column. The seconds are counted in a `TIME`, `DATETIME` or `DATE` column or a
written time, and a time is written out of a whole-number column or a written
whole number. A `TIMESTAMP`, read in the session's zone, a word column and a
written value MySQL warns about — a word naming no time, a count past the
widest time — are refused. `ADDTIME` and `TIME_FORMAT` are not taken, and
`MAKETIME` and `MAKEDATE` are taken over written values only.

`ADDDATE` and `SUBDATE` are `DATE_ADD` and `DATE_SUB` under other names, and
a bare count is a count of days: `ADDDATE(d, 1)` is `DATE_ADD(d, INTERVAL 1
DAY)`. Measured on 8.4.11, each reports what its `DATE_ADD` spelling reports,
and is taken wherever that spelling is taken as a result column. A count read
from the row is refused, as it is for `DATE_ADD`.

A `LIKE` pattern may be written in pieces — `LIKE CONCAT('%', ?, '%')` is how
a search filter wraps the value it binds in wildcards, and
`LIKE CONCAT('%', 'lph', '%')` the same pattern written out. Measured on MySQL
8.4.11 over 'alpha', 'beta', 'ALPHABET' and 'gamma', both answer rows 1 and 3,
which is exactly what `LIKE '%lph%'` answers: the pieces spell one pattern,
matched without regard to case. `NOT LIKE` answers the rest, and the shape
holds in a statement that writes.

Pieces that are all written are joined into the one pattern they spell, so the
engine is handed the pattern it would have been handed anyway. A bound piece
cannot be joined before it binds, so there the pieces are joined for the engine
instead. The backslash rule holds over each piece, for the reason it holds over
a whole pattern: MySQL reads one as an escape and the engine reads it as
itself. A piece naming a column is refused — the pattern would be a different
one for every row — and so is a second bound piece, one bound value being all a
checked comparison records.

A subquery answering one value stands where a value stands, which is how a
statement asks for the row holding the highest of something —
`WHERE n = (SELECT MAX(n) FROM t)` — or whether another table holds anything at
all — `WHERE (SELECT COUNT(*) FROM c) > 0`, written either way round. Measured
on MySQL 8.4.11 over parents (1,5), (2,3), (3,9) and children pointing at 1 and
3, the engine answers each the same way, and the subquery may carry its own
`WHERE`. It holds in an `UPDATE` and a `DELETE` too, over a table the statement
does not change; one reading the table it does change is 1093 there and refused
here, as every DML subquery is.

What makes the shape safe is that an aggregate over one implicit group answers
exactly one row. A plain column does not, and MySQL says so: measured,
`id = (SELECT parent_id FROM child)` over two child rows answers 1242 where the
engine takes the first row it finds, so that projection is refused. So is a
grouped subquery, which answers a row per group.

A plain column is taken when the subquery picks its row by a key, which can
never find two: its `WHERE` fixes, through `AND`s alone, each column of the
table's primary key, or of a unique key over columns that are never NULL, to
one written or bound value. That is how Prisma pages from a cursor —
`posts.id >= (SELECT posts.id FROM posts WHERE (posts.id) = (?))`. Measured on
8.4.11: cursor 2 answers posts 2 and 3, one bound as the word `'2'` the same,
and a cursor naming no row answers nothing, the comparison meeting NULL. The
column read is held to the kind of the column it meets, as a `MIN` is, and a
column meeting it through a table name has to be one of the one table the
statement reads. A prepared statement is held to that pair rule now as well,
and to the `IN (SELECT ...)` one, which only a statement sent as text was
held to before.

A `MIN` or `MAX` answers its column's own kind, so the two columns are recorded
and held to the rule `column IN (SELECT column ...)` holds its pair to — a word
column against a number aggregate is refused, which is the comparison MySQL
answers by coercion, with a 1292 warning per row. A `COUNT` answers a whole
number whatever it counts, so it meets a whole number written out and nothing
else. `SUM` is left out, not having been measured against a column.

An `AVG` is taken against a column, which is how a statement asks for the rows
above average — `WHERE views > (SELECT AVG(views) FROM posts)`, written either
way round, with `=`, `<>`, `<`, `<=`, `>` or `>=`. MySQL answers the average of
whole numbers as a decimal rounded to four places and compares the column
against that decimal, where the engine's own `AVG` keeps the whole fraction as a
float, so a row could land on the other side of it. So the average is taken as
the engine's exact `mysql_decimal_avg`, which answers MySQL's decimal, and the
two sides are compared with `numeric_lt` and `numeric_eq`, which compare exact
numbers and answer NULL where either side is NULL — an average over no rows
keeps no row, as in MySQL. Measured on 8.4.11 over views 10, 3, 7, 0 and 5 and
over 1, 1 and 2, the engine keeps the rows MySQL keeps for each operator. Both
columns are held to whole numbers; a `DECIMAL`, a float or a word on either
side is refused, and so is a correlated average, whose qualified argument the
aggregate reader does not take. A statement whose tables hold a `DECIMAL` and
that needs its columns' types for another reason — an `ORDER BY` a bare column
— is refused as any such statement with a subquery is.

A comparison naming no column is taken when both sides are whole numbers, and
so is a bare whole number standing as the whole predicate. That is the opening
a statement built up in pieces writes — `WHERE 1 = 1 AND ...` — so each piece
after it can carry an `AND` in front and none of them has to know whether it is
first. Measured on MySQL 8.4.11 over three rows: `1 = 1` and `2 > 1` keep every
row, `1 = 0` and `1 <> 1` keep none, and the bare `WHERE 1` and `WHERE 0` read
the same way; the engine answers each identically, so the comparison is written
out as it stands rather than checked against a column that is not there. It
holds in an `UPDATE` and a `DELETE` as well.

Three neighbouring shapes keep the refusal every uncalibrated comparison has,
because MySQL answers them by rules the engine does not follow: `'a' = 'A'` is
true there, the collation reading two words without regard to case; `1 = '1'`
is true, the word coerced to a number; and a number written with a fraction has
not been measured here. `NULL = NULL` is false in both, but is written as a
comparison rather than as `IS`, and stays refused with the rest.

A word naming a whole number against a column holding whole numbers is read as
that number, which is how PHP writes every value: WordPress's
`$wpdb->prepare('%s')` and PDO with emulated prepares send `WHERE id = '1'`, and
Laravel binds what it reads from a request as a word. Measured on 8.4.11 and
matched over `=`, `<>`, `<`, `IN`, `NOT IN` and `BETWEEN`, in a `SELECT`, an
`UPDATE` and a `DELETE`: MySQL reads the word as exactly the number —
`big = '9007199254740993'` over a `BIGINT` finds that row and not the one
holding 9007199254740992, which a comparison between doubles would also find —
and a sign or leading zeroes name the same number, `'+3'`, `'03'` and `'-0'`
among them. Against a `DECIMAL` a word naming a decimal is read exactly too:
`balance = '10.5'` finds 10.50, and `'10.500000000000000001'` finds nothing. A
word bound as a string against a whole-number column is bound as its number,
and `YEAR(created_at) = '2026'` reads the word against the whole number the
call answers — MySQL compares those as doubles, so only a number nearer zero
than 2^53 is read there. Each is rendered a second time knowing the column's
type, the way a written day is. A `SELECT` over several tables reads such a
word the same way, in a join's `ON` as in its `WHERE` — TypeORM loads a
relation with `` INNER JOIN `post_tag` `t` ON (`t`.`tag_id` = '4' AND ...) ``
and reads a page back with `` WHERE `Post`.`id` IN ('1') `` over a `LEFT
JOIN`, measured on 8.4.11 and matched — and refuses one whose column name
holds whole numbers in one of its tables and another kind in another.

Every other word keeps the refusal: MySQL reads `age = '1.5'` as a comparison
between doubles, `' 30'`, `'30 '`, `'3e1'` and `''` without a warning by rules
not spelled out here, and `'30abc'`, `'abc'` and `'0x1E'` with warning 1292. A
word against a `DOUBLE`, a word past what an `i64` holds, and a word under an
explicit `COLLATE` are refused too.

A `?` compared with a column of words in a prepared `UPDATE` or `DELETE` —
Laravel's `update cache set value = ? where key = ?` and `delete from
migrations where migration = ?`, and Eloquent's `update users set ...,
users.updated_at = ? where email = ?` — binds a word compared under the
column's collation, as a prepared `SELECT`'s does: the statement is read a
second time knowing which of its table's columns hold words. Measured on 8.4.11
over `utf8mb4_unicode_ci`: `key = ?` bound `'LARAVEL-CACHE-COUNTER  '` finds
`laravel-cache-counter`, `key IN (?, ?)` matches the same way, and a `?` in the
`SET` is counted before those of the `WHERE`. A `?` inside a subquery is held
the same way, by the type of the column it meets — Laravel's `->exists()`,
`firstOrCreate` and `unique` rule send `select exists(select * from posts where
title = ?) as exists`, which MySQL answers as a NOT NULL binary `LONGLONG` of 1
— where it used to be refused for want of that column's type. A number bound there is refused,
in a prepared `SELECT` too, where it used to be compared as the word it spells:
MySQL compares a column of words with a number as the number each word begins
with — `name = 0` finds `'abc'` and `name = 5` finds `'5x'`.

`IFNULL` and `COALESCE` take an aggregate as the thing they default, which is
how a report asks for a total over rows that may not be there —
`IFNULL(SUM(n), 0)` — or for the highest of nothing —
`COALESCE(MAX(id), 0)`. Measured on MySQL 8.4.11, each answers the shape its
aggregate answers on its own: the NEWDECIMAL of length 33 that `SUM` over an
INT answers, the length 16 and scale 4 of `AVG`, the length 34 and scale 2 of
`SUM` over a `DECIMAL(10,2)`, the length 21 of `COUNT`. The measured DECIMAL
shape is not yet supported: `IFNULL(SUM(decimal_column), 0)` and its `COALESCE`
form are refused until the fallback and aggregate can be combined without
losing exact digits. What the supported wrapper adds
is NOT_NULL, and a widening of any whole number to a BIGINT that leaves the
length alone — `IFNULL(MAX(s), 0)` over a `SMALLINT` answers LONGLONG with the
SMALLINT's length 6. That is the same widening `IFNULL` over a plain column
does, so the two forms are one rule. Over no rows at all the answer is the
fallback rather than NULL, which is the whole reason the call is written.

The count may also have been worked out by a derived table the statement
reads, and be named through it: Prisma counts a relation with
`COALESCE(aggr_selection_0_Post._aggr_count_posts, 0)` over a `LEFT JOIN` of
`(SELECT user_id, COUNT(*) AS _aggr_count_posts FROM posts GROUP BY user_id)`,
so a user with no posts has none to count. Measured on 8.4.11, `COALESCE` and
`IFNULL` there answer the count's own shape whatever whole number they fall
back on — a NOT NULL `LONGLONG` of 21 with the binary flag, naming no table —
and the users' counts 2, 1 and 0. Any other column a derived table worked out,
a total among them, and any table's own column named through its table are
still refused with a fallback.

The fallback still has to be a whole number, which is the rule the plain-column
form follows. A call inside the call rather than an aggregate, and `NULLIF`,
which answers the other way round, are refused.

An `ON DUPLICATE KEY UPDATE` value may join the row already there to the one
offered, which is how a counter is stepped: `hits = hits + 1`, or
`hits = hits + VALUES(hits)` to step it by the number offered. A bare column is
the row already there in both MySQL and the engine, and `VALUES(col)` is the
offered one, which the engine calls `excluded.col`.

MySQL 8.0.20 deprecated `VALUES(col)`, and TypeORM and GORM still write it.
Measured on 8.4.11, each call written raises warning 1287 once, however many
rows the statement offers, and a prepared statement raises it when it is
prepared rather than each time it is executed. This raises the same warnings,
at the same points, and `SHOW WARNINGS` lists them.

Since MySQL 8.0.19 the offered row may carry a name instead — `VALUES (...) AS
offered ... hits = t.hits + offered.hits` — which is the spelling that replaces
`VALUES()`. Measured on 8.4.11 over (1, 10, 'a') and (2, 20, 'b'), running the
whole sequence and matched row for row: stepping row 1 leaves 11, a row that is
not there is inserted as it stands, row 2 goes 20 to 27 to 30, and a word taken
from the offered row lands as written.

Once the offered row carries a name, a bare column is 1052 — ambiguous between
the two rows — so only a qualified one names either, and the bare form is
refused here as it is there. A qualifier naming neither row is refused, and so
is renaming what that row carries, which has not been measured.

The clause takes its assignments left to right, as an `UPDATE`'s `SET` does:
measured on 8.4.11, `a = a + 10, b = a` over a row holding 1 and 1 leaves 11 and
11, where the engine reads the row as it stood and would leave 11 and 1. So a
value reading a column of the row already there that an earlier assignment of
the same clause wrote is refused, and so is writing one column twice. Reading a
column before the clause writes it answers the same in both — `b = a, a = a +
10` leaves 11 and 1 in MySQL too — and the offered row is never written, so
`VALUES(a)` and `o.a` are read anywhere in the clause.

Rails 8's `upsert_all` touches a row's `updated_at` only when it changes one of the
columns it names: `updated_at = (CASE WHEN (users.name <=> users_values.name AND
users.balance <=> users_values.balance) THEN users.updated_at ELSE CURRENT_TIMESTAMP(6)
END)`, first in the clause, before the columns it compares are written. That shape is taken:
one condition joining `<=>` comparisons of a column of the row already there with the same
column of the row offered, the column written kept in one branch and a reading of the clock in
the other. `<=>` is the engine's `IS`, which compares a column of words under the column's own
collation as MySQL does — measured on 8.4.11, offering `'BOB'` over `'Bob'` writes the name,
counts 2 and leaves the timestamp, and offering the same rows twice counts 0 the second time.
MySQL puts the offered value into the column's type before it compares and the engine compares
it as it was written, so a comparison is taken only where the two answer alike: a word offered
for a `VARCHAR` or `TEXT`, a whole number for an integer column, a written number for a
`DECIMAL`, which the engine puts into the column's form first. Anything else is refused —
measured, `'2026-01-01'` offered for a `DATETIME` holding that midnight is the same moment in
MySQL, and a different word here — and so is a bound value, whose kind is not known until it
is bound, and any other `CASE` in the clause.

A name on the offered row with no upsert to use it names nothing, and Rails 8
writes one on every `insert_all!` — `INSERT INTO tags (name) VALUES ('x') AS
tags_values`. Measured on 8.4.11, such an insert writes, numbers its rows and
answers 1062 for a duplicate exactly as the same insert without the name does,
on a table that counts its own ids and on one that does not, so the name is
dropped.

An `UPDATE` may write a value worked out from the row rather than one written
out: a word joined, lowered, trimmed or cut short, a number with a fallback or
counted from a word, a word chosen by a `CASE`. Each is rendered the way a
projection renders it, so what lands in the column is the value that reading
answers, and only the readings this frontend already measures are taken.
Measured on MySQL 8.4.11 over (1, 5, ' Alpha ') and (2, NULL, 'beta'), running
the whole sequence, and matched row for row and count for count.

What is refused is a value reading a column the same `SET` has already written.
Measured, MySQL takes the assignments left to right: after `SET name = 'q',
n = CHAR_LENGTH(name)` the row holds 1 and 'q', the length of the word just
written, where the engine reads the row as it stood and would answer 5. That is
a difference in the answer rather than in the shape, so the whole statement is
turned away. A value whose shape this cannot read through is refused with it,
rather than let past unread.

An `UPDATE` or `DELETE` may name its rows through a subquery — `WHERE id IN
(SELECT ...)`, `NOT IN`, or `EXISTS` — which is how a test fixture clears out
whatever another table points at. Measured on MySQL 8.4.11 over parents
(1,10), (2,20), (3,30), (4,40) with children pointing at 1, 3 and nothing:
the `IN` form changes two rows, an `EXISTS` correlated back to the row being
changed finds the same two, and `NOT IN` matches nothing at all, because the
child holding nothing makes every test unknown. The engine answers each the
same way.

The subquery's table is a table the statement reads, so it is authorized
alongside the one being written and the internal catalog stays out of reach
through it. Its comparisons are checked against it while the statement's own
unqualified comparisons are checked against the table being written, which the
statement does not read — that is the split `WHERE n > 5 AND id IN (SELECT
...)` needs. The column an `IN (SELECT ...)` compares is held to the rule a
`SELECT` holds it to, both sides the same kind, since MySQL coerces where the
engine compares by affinity.

What MySQL refuses is a subquery reading the table being changed: 1093, the
target table named in a FROM clause. The engine would answer it, so it is
refused here rather than answered differently.

A wildcard may be qualified — `SELECT a.*` — which is how a joined statement
takes a whole row from one side. Measured on MySQL 8.4.11, it answers that
source's columns in declaration order, each naming its own table, and the
engine spells it the same way. It mixes with a plain column and with a second
wildcard, and every column keeps the metadata it would carry on its own: on the
outer side of a `LEFT JOIN` the unmatched row answers NULL throughout and the
NOT_NULL flag is dropped, and under an alias the columns report the alias as
their table and the real name as the original table. The alias is reported as
written, as MySQL reports it — measured, `FROM posts Post` names `Post`; the
engine reads an alias in lower case, and `post` used to be reported for any
column read under one. Naming the table an alias
renamed — `SELECT a.* FROM a AS t` — answers 1051 in MySQL and is refused here
too. A qualifier carrying a schema, `db.t.*`, is answered by MySQL but refused
here, being a source name that has to be resolved across databases first.

A grouping key may be qualified or not, and matches a projection either way,
which is what MySQL does whenever the bare name is unambiguous; the engine
answers the ambiguous case itself. A grouping key that is an expression is
taken for the calls the `GROUP BY` paragraphs above list. A text column carries
its stored UCA9 collation into the grouping key, so `abc`, `ABC` and accent
variants share a group as in MySQL.

`WITH ROLLUP` is taken over one to three keys that are whole columns: `SELECT
status, COUNT(*) FROM users GROUP BY status WITH ROLLUP` answers each group and
then a total of every row with `status` answered as NULL. The engine has no
`ROLLUP`, so the statement is written out as what it means — one grouped
`SELECT` for each level, the rolled-up keys answered as NULL, joined with
`UNION ALL` — and ordered the way MySQL answers it. Measured on MySQL 8.4.11:

- The rows come sorted by the keys in the order the `GROUP BY` names them, NULL
  first, and each super total follows the groups it totals, the grand total
  last. A group whose key is NULL sorts among the groups and a super total after
  them, which is how the two NULLs are told apart. Words sort and group the way
  MySQL compares them — `active` and `Active` are one group, before `Bob`.
- A key reports its column's type and length, names no table and no column, and
  is nullable whatever the column is: a whole number and a `DECIMAL` carry the
  binary flag and their unsigned one, a `TINYINT(1)` reporting a `TINYINT`'s 4;
  a `VARCHAR` and a `CHAR` carry no flags and 31 decimals. An aggregate reports
  the shape it reports without the rollup, words with 31 decimals.
- A `HAVING` filters the totals as well as the groups, a `LIMIT` counts them,
  and over no rows there is no grand total either.

Refused: an `ORDER BY` beside the rollup, which MySQL answers through a
temporary table with shapes of its own; `GROUPING()`; a key that is an
expression; a key of any other type, and a largest or smallest moment, which
answers words of 76 under a rollup; a `HAVING` naming a key; a `?`, which each
level would read again; and more than three keys.

`DISTINCT` is taken, and `DISTINCTROW` with it, since that is MySQL's own
synonym. It drops repeats among the projected values and leaves the result
metadata exactly as it would be without it. A text column uses the same UCA9
weights as `WHERE` and `GROUP BY`, so `SELECT DISTINCT name` collapses case and
accent variants. `DISTINCT ON` is refused, being no part of MySQL.

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
real to fix that, and the result is rendered at its declared scale like any
other decimal, so `3/2` reads back as `1.5000` as it does in MySQL. Its
metadata is measured:
precision is the left operand's plus four, scale is four, and the length adds
one for the sign and one for the point, so `3/2` is 7 and `i / 2` is 16. A
division is never NOT NULL, because dividing by zero answers NULL in both.

An unaliased expression column is named after the source text, spacing and
parentheses included, which is what MySQL does — `1+1` keeps its spelling where
the engine would print `1 + 1`. That name comes from the statement rather than
the AST for exactly that reason.

Exact `DECIMAL` arithmetic is taken for a known decimal column or its aggregate
combined with a written numeric literal through `+`, `-`, `*` or `/`, and for
`+`, `-` or `*` with a known integer column or aggregate. The literal's digits
are passed as text to exact arithmetic; multiplication rounds to MySQL's
maximum scale of 30, and division answers at `min(column scale + 4, 30)`
places. An expression mixing a FLOAT/DOUBLE column with DECIMAL follows MySQL's
floating-point result path. Division by a written zero, division with the
literal on the left, nested or untyped decimal expressions, and a decimal
column operand with no `FROM` remain refused.

An integer result that leaves `BIGINT`'s range answers 1690 / 22003, as MySQL
does — measured. The engine turns the same sum into a float rather than failing,
and a float where a column promised an integer is exactly that overflow, so both
protocols answer the error rather than sending a number the column's type does
not describe.

A window, a filter, more than one argument and an
expression argument stay refused; DISTINCT on aggregates other than COUNT remains refused;
`GROUP_CONCAT` with `ORDER BY` or `DISTINCT` stays refused; a `SEPARATOR` is
taken.

`REPLACE INTO` is taken, over the same `VALUES` shape an ordinary `INSERT`
takes. MySQL's `REPLACE` deletes the rows a unique key collides with and
inserts, which is what the engine's own `OR REPLACE` does, so the rows it leaves
behind are MySQL's. The affected count is the number inserted plus the number
actually deleted: measured on 8.4.11, a new row counts 1, a replaced one counts
2, and a new row plus one replacement counts 3. A replacement that deletes two
rows through different unique keys counts 3. Everything an ordinary `INSERT` refuses —
`ON DUPLICATE KEY UPDATE`, `IGNORE` — a `REPLACE` refuses too, and everything it
takes, the `SET` form included, a `REPLACE` takes.

`INSERT ... ON DUPLICATE KEY UPDATE` is taken. It is an upsert, and the engine
has the same one: MySQL's clause fires on a collision with any unique key, and
the engine's `ON CONFLICT DO UPDATE`, written without a conflict target, does
too. `VALUES(col)` names the value the row was offered, which the engine spells
`excluded.col`. The columns the clause does not name are left as they were, in
both.

The affected count is MySQL's, which took an engine change of its own: the row
count alone cannot tell a written row from a row written over, being one either
way, so the engine says how many rows a statement wrote over and how many of
those actually changed. What each of the three counts is set out further down,
where that change is described.

A table with an `ON UPDATE CURRENT_TIMESTAMP` column — TypeORM's `@UpdateDateColumn` — takes
an upsert the clause of which leaves that column when the table counts its own ids and the
upsert asks the counter for every id or has several rows, which are the upserts written one
row at a time. Measured on 8.4.11, MySQL writes the moment into the column whenever the upsert
changes the row — a name differing only in case or by a trailing space included — and leaves
it when the row stands as it was, a `DECIMAL` offered as `'100.0'` over 100.00 among them. The
engine's upsert writes nothing there, so each row is written, and where the engine counts the
row it met as changed — it compares the row it wrote with the one that stood, which is what
decides the affected count too — the row is written again in its place with the moment in the
column, read once for the whole statement. TypeORM's `repository.upsert` is answered as MySQL
answers it, prepared or not. Refused still: the same upsert on a table that does not count its
ids, whose rows are written in one statement; one row naming its own id; a session whose time
zone is not UTC; and a table carrying a trigger, which the second writing would set off twice.
An upsert whose clause writes the column itself is taken everywhere, as before, and answered
with what the clause says.

A row left as it stood is neither counted nor stamped whichever write put it there — an
`INSERT`, an upsert or an `UPDATE`. The engine's `UPDATE` used to store a counted row's id
inside the row where an `INSERT` stores nothing, as SQLite does, so the first `UPDATE` of a row
an `INSERT` wrote counted it as changed, and the first upsert of a row an `UPDATE` wrote counted
and stamped it; the engine now stores the row alike after each.

The clause is refused where it would be dropped rather than answered: on the
`SET` form and the empty-row form, which leave no room for it, and beside
`REPLACE` or `IGNORE`, which already decide what a collision does. On a table
that counts its own ids it is taken over one row and refused over several.

`INSERT ... SELECT` is taken. The SELECT goes through the same translator a
bare one does, so it is held to the same rules, and the rows it reads are the
rows that get written — measured on 8.4.11 over (1,10),(2,20),(3,30),
`INSERT INTO dst (id, n) SELECT id, n FROM src WHERE n > 15` writes two rows and
counts 2, which this matches.

The part that had to be built rather than reused is authorization. A statement's
tables were found by asking the SELECT parser, and an `INSERT ... SELECT` is not
a SELECT, so it would have answered nothing: with database-wide `Query` granted
the table it reads would have gone unauthorized and unchecked against the
internal catalog. A DML statement now carries the tables it reads, and they are
authorized the way a SELECT's are, so `INSERT INTO dst SELECT ... FROM
sqlite_schema` is refused.

Written with no column list, the statement means every column of the table in
order, which is what MySQL makes it — measured on 8.4.11, `INSERT INTO dst
SELECT * FROM src` copies all three columns. The list is written out where the
table is known and the ordinary statement runs, so the read table is authorized
and checked the same way and nothing else changes. A `SELECT` answering a
different number of columns is refused rather than written into the wrong ones;
MySQL answers 1136 there. `IGNORE` and an upsert clause are refused with the
form, as they are wherever they are written.

The `WHERE` inside is checked against the table the SELECT reads rather than the
one the INSERT writes, which is the table it actually compares against. A SELECT that has to
know its columns' types to be rendered — one ordering a bare column, or comparing a `?` — is
rendered the way a bare `SELECT` is, read a second time knowing them, and the copy takes what
that reading renders. Measured on 8.4.11, Laravel's `insertUsing` arrives prepared as
`insert into t (a, b) select a, b from u where c = ?`, and its `SELECT` finds the rows a bare
`SELECT` binding the same value finds: a word matching without regard to case, a bound day
meeting a `TIMESTAMP`. A bound value there is held to the column it meets as a bare prepared
`SELECT` holds it, a `LIMIT ?` included, which takes a whole number only. One ordering a text
column copies the rows in the order MySQL answers them, without regard to case. A prepared
statement read that way is read again only while the tables it names stand as they did; once
one changes it answers an error and is prepared again.

`INSERT IGNORE` is taken, over both forms, and skips a row whose key collides
instead of failing the statement — the engine's own `OR IGNORE`. Measured on
8.4.11 over a table already holding row 1: inserting row 1 again leaves the
stored row alone and counts 0, and a two-row statement where only the second is
new counts 1. The engine answers the same for both.

What MySQL's IGNORE also does is coerce a value it would otherwise refuse, and
that is not done here. Measured: `INSERT IGNORE` of 99999999999999 into an `INT`
stores 2147483647, where this refuses the statement — an error rather than a row
holding a number the client did not write. A NULL is the one case where the two
IGNOREs part company silently: MySQL stores a coerced 0 in a NOT NULL column
while the engine's `OR IGNORE` skips the row and stores nothing, so an
`INSERT IGNORE` that writes a NULL is refused outright rather than left to
disagree. That refuses a NULL bound for a column that accepts one too, which
would have agreed; the column is not known where the refusal is made.

An `INSERT ... ON DUPLICATE KEY UPDATE` reports what it did to each row rather
than how many it touched, which is a rule of MySQL's own. Measured on 8.4.11 and
matched: one for a row it wrote, **two** for a row it changed, **zero** for a row
it left as it stood, and the sum of those over a statement writing several rows —
one written beside one changed reports three.

The row count alone cannot tell the three apart, being one either way, so the
engine says two more things now: how many rows a statement wrote over a row
already there, and how many of those actually changed. The second was already
counted for an `UPDATE` and simply had not been asked for on an upsert's update;
the first is new. Neither is visible to a statement that is not an upsert — a
plain `INSERT`, `UPDATE` and `DELETE` are counted the way they always were.

An upsert on a table that counts its own ids is taken. MySQL burns the number
the row would have taken — measured on 8.4.11, an upsert that updated a row
leaves the next plain insert two numbers on, and the table prints
`AUTO_INCREMENT=5` where three rows stand — which is what the allocator here
does anyway, reserving before it writes and never going back.

The id such a statement reports back is the harder half: for an upsert that
updated, MySQL's OK packet reports the id of the row it *updated*, and the
frontend holds only the number it reserved. Which row the upsert matched is
decided inside the engine, where several unique keys could have matched, so the
engine records that row's own number as it writes over it and the frontend
reports that. `LAST_INSERT_ID()` is left where it stood, measured, so it still
reads the number the statement before it took. An upsert that matched a row and
left it as it stood reports no id at all — measured, affected rows 0 and id 0 —
where this used to report the matched row's.

A statement of several rows is written one row at a time, and what it reports
follows what each row did. Measured on 8.4.11: a statement that added a row
reports the first number it added, `VALUES ('b', 2), ('a', 3)` where only the
second matches reporting the id of the row it *wrote*; a row that matched gives
its number back to the next row the statement adds, so `('c', ...), ('e', ...)`
where `c` matches writes `e` with the number `c` would have taken; one that added
no row but changed one reports the id of the last row it matched, changed or
not; and one that changed nothing reports 0. A prepared statement of several rows
— which is how Laravel, preparing everything, sends `upsert()` and `insertOrIgnore()` —
is written the same way, each row binding the values its own `?`s and the upsert clause's
name, and reports the same: measured, over bound values exactly as over written ones. The
[oracle case](conformance/cases/p0/insert-counted-upsert.json) pins the single row.

A `BIGINT UNSIGNED` counted column takes an upsert as well, which is the key
Laravel counts every table with and its `upsert()` writes against. That column
is not the engine's own row number — the row number cannot hold its upper range
— so the id of the row an upsert matched is read back off that row, and reports
exactly past the engine's signed range: measured, a row numbered
18446744073709551001 is reported as that number.

The clause may not change the counted column, with one exception: writing it to
itself, which is how GORM spells doing nothing on a collision — `ON DUPLICATE
KEY UPDATE id = id`, prepared — and how Rails' `insert_all` does when the id is
its first column, `id = tags.id`. Measured on 8.4.11 over GORM's `BIGINT
UNSIGNED` table and a signed one, it is an upsert that leaves the row as it
stood: 0 rows and no id, `LAST_INSERT_ID()` left where it was, the number the
row asked for spent, and a session counting found rows counting the row 1 and
still reporting no id. This reports the same.

Rows of such an upsert may name their own ids, which is how GORM writes an
association: `Append(&goTag, &Tag{Name: "news"})` upserts `(name, id) VALUES
(?, ?), (?, DEFAULT)`, naming the id of a tag it read back beside a new one, and
`Replace` names both. Measured on 8.4.11 with go-sql-driver and matched: a row
naming an id at or below the counter leaves the counter where it is whether it
is written or collides; the rows asking for a number take the statement's whole
batch at the first of them, a colliding one handing its number on as above; a
statement that added a row asking for a number reports the first such number
and sets `LAST_INSERT_ID()` to it; one that added none but wrote some row — one
naming its own id, or one it changed — reports the id of the last row it met,
written, changed or left as it stood, the id of the row already there for one
that collided; and one that wrote nothing reports 0, leaving
`LAST_INSERT_ID()` alone. Rows that all name their own ids may name ids past the
counter too, which moves past each one once its row is written and past none
that collides — measured, `(50, ...), (60, ...)` where the second collides on
another key leaves the next number at 51. A row naming an id past the counter
beside one asking for a number is refused: MySQL moves the counter as the rows
go by, so the number that row takes depends on the ones before it, and the
numbers here are reserved before any row is written. `IGNORE` over several rows
naming ids stays refused.

Rows of such an upsert may also give a column `DEFAULT` in some rows and a value
in others, which is how TypeORM's `repository.upsert` writes entities that set
different columns — `VALUES (DEFAULT, 'alice@example.com', 'Alice', '100.00'),
(DEFAULT, 'erin@example.com', 'Erin', DEFAULT)`. Each row is written by a
statement of its own, which leaves out the columns that row gives `DEFAULT`, so
the row takes the column's own default and so does the row offered to the
clause: measured on 8.4.11 and matched, `balance = VALUES(balance)` over a row
offering `DEFAULT` writes the column's default, 0.00. A table that does not
count its own ids writes the rows in one statement, which cannot leave a column
out of one row only, so there it is refused.

`INSERT IGNORE` into a table that counts its own ids is taken over one row.
The allocator reserves its range before the rows are written, so a row `IGNORE`
skips has already taken a number — and that is what MySQL does too: measured on
8.4.11, the counter moves past a skipped row exactly as it moves past a written
one, so the table prints `AUTO_INCREMENT=3` where one row stands and one was
skipped. What such a statement reports is measured now as well: a skipped row
counts 0 and reports no id at all, leaving `LAST_INSERT_ID()` where it stood,
and a written one counts 1 and reports the number it took.

A statement of several rows is refused there, the way an upsert of several is:
which of them the reported id comes from depends on what each of them did.

`REPLACE INTO` on such a table is taken as well, and always takes a new number,
the replaced row being deleted and a new one written — measured, replacing the
row numbered 1 leaves it numbered 4 where the counter stood at 4. Its affected
count includes the deleted and inserted rows, two for this case. The [oracle
case](conformance/cases/p0/insert-counted-replace.json) pins all of it.

`INSERT ... SET a = 1, b = 2` is taken. It names its columns and values in one
place instead of two and means what the column-list form means — measured on
8.4.11, `INSERT INTO s SET id = 1, a = 2, b = 'x'` stores the row
`INSERT INTO s (id, a, b) VALUES (1, 2, 'x')` stores, and a column the SET
leaves out takes its default. It is rendered as the other form rather than given
rules of its own, so one set of rules covers both: the same value kinds, the
same required-column check, the same refusals. An `AUTO_INCREMENT` table takes
it too: the allocator reads only the column-list form, so the SET one is written
out as that before it reaches the allocator, where the table is known. Measured
on 8.4.11, `INSERT INTO ai SET v = 1, s = 'a'` numbers the row 1 and
`LAST_INSERT_ID()` answers 1, which this matches, and naming the key itself is
refused on both forms alike — the allocator reserves before the row is written,
so a row carrying its own key would not go through it.

An upsert clause comes along with it. `INSERT INTO k SET id = 1, v = 20 ON
DUPLICATE KEY UPDATE n = 999` says what happens to a row that collides, which is
the same whichever way the row itself was written — measured on 8.4.11, it
leaves `v` at what the row already held and writes `n`, exactly as the
column-list form with the same clause does. On an `AUTO_INCREMENT` table it is
refused on both forms alike, because the allocator reserves before the clause
can turn the row into an update.

What is refused is mixing the forms, `INSERT INTO t (a) SET a = 1`, which is not
MySQL syntax.

A row written into a table that counts its own ids takes the readings of the clock an
ordinary `INSERT` takes — `NOW()`, `CURDATE()`, `CURTIME()`, their other spellings, and one of
them shifted by an interval — so `INSERT INTO users (name, created_at, updated_at) VALUES
('Fi', NOW(), NOW())` is numbered like any other row, beside written values and bound `?`s
alike. Measured on 8.4.11, three such rows into an empty table report 1, two more writing
NULL and 0 into the id beside `NOW()` take 4 and 5 as they would anywhere else, and the table
then prints `AUTO_INCREMENT=6`. MySQL reads the clock once for the whole statement, and a
statement this writes one row at a time — several rows with `IGNORE` or `ON DUPLICATE KEY
UPDATE`, and rows asking for the next number beside one naming its own id past the counter —
reads it once between all its rows: measured, forty rows stamped with `NOW(6)` beside an upsert
hold one moment, where the engine's clock, reading to the millisecond, would have run past
several.

`INSERT INTO t (a, b) SELECT ...` into a table that counts its own ids is taken — Laravel's
`insertUsing` and a data migration's copy both write it. The `SELECT` is held to the rules an
ordinary `INSERT ... SELECT` is, and every row it answers is read before any is written, which
is what MySQL does when the `SELECT` reads the table being written: measured on 8.4.11,
`INSERT INTO a1 (name) SELECT name FROM a1` over 11 rows copies 11. The rows take the next
numbers in the order the `SELECT` answers them, the statement reports the first, and
`LAST_INSERT_ID()` answers it. The counter does not stop at the last number used: MySQL cannot
know how many rows a `SELECT` answers, so it takes numbers in batches of 1, 2, 4 and on up to
65535 and spends what the last batch leaves over — measured, 1 row moves the counter on by 1,
3 rows by 3, 4 rows by 7, 8 rows by 15, and 9 rows into an empty table leave it at
`AUTO_INCREMENT=16`. The same batches are reserved here, so the next row takes the number it
takes there. A `SELECT` answering no rows writes none, reports 0, and leaves the counter and
`LAST_INSERT_ID()` alone.

A copy that lists the counted column follows what `VALUES` rows do with it. NULL, and 0 unless
`sql_mode` names `NO_AUTO_VALUE_ON_ZERO`, ask for the next number; rows naming their own ids
raise the counter past the highest and report the last row's, leaving `LAST_INSERT_ID()` where
it stood — which is how `INSERT INTO copy SELECT * FROM users` keeps every id. A statement mixing
the two is refused: measured, MySQL takes a new batch whenever a written id passes the batch it
holds, so the counter's landing place depends on the rows in an order this does not repeat. A
required column the copy leaves out answers 1364 once there is a row to write, and before a
number is spent; with no row, measured, there is no complaint. `IGNORE`, `REPLACE` and
`ON DUPLICATE KEY UPDATE` beside a `SELECT` are refused on such a table, as is a copy in a
session whose time zone is not UTC and one whose batch of numbers would pass the column's
highest — MySQL cuts that batch short and writes the rows that fit. A prepared copy — how
Laravel's `insertUsing` arrives, its bindings in the `SELECT`'s `WHERE` — is taken: nothing is
reserved when it is prepared, and when it runs its `SELECT` binds the values it was given and
is read before a number is taken, exactly as the text statement is.

A row found wrong while it is filled takes no number. Measured on 8.4.11: a broken `CHECK`,
a NULL for a `NOT NULL` column, and a value too long, out of range or of the wrong kind each fail
the row before it takes its number, so the next row takes the number the failed one would have;
a duplicate key and a missing parent row fail once the row is written, having spent it; and of
several rows, one failing after the first spends the statement's whole batch while a failing
first row spends nothing. A `VALUES` insert whose every row leaves its id to the counter, into a
table carrying no trigger, now matches all of it. The counter hands out a number once and never
takes one back, so the rows are written first, inside a savepoint, with the numbers the counter
would hand out next, and the numbers are taken afterwards — never, when the first row failed to
fill. Should another session take those numbers in between, the statement is undone and written
again with the numbers it was actually handed, so no row is ever written with a number the
counter did not hand this statement. The other counted paths reserve before they write, and still
spend a number on such a failure; see TODO.md.

A table that counts its own ids may carry a trigger, and a trigger may write into one — a
restored `mysqldump` of a blog carries exactly that, each new post writing a row into an audit
table numbered the same way. The statement's own rows are numbered as always, and the trigger
reads each row's number in `NEW.id`. A row a trigger writes into a counted table takes that
table's next number at the moment it is written: the engine asks the session's counter for it,
where it would otherwise number the row one past the highest the table holds. Measured on
8.4.11 and matched: a trigger's rows take numbers one at a time — two rows of one statement
take two consecutive numbers there, whatever batch the statement took for its own table — a row
an upsert turned into an update or `IGNORE` skipped fires no trigger and spends nothing there,
and neither the id the statement reports nor `LAST_INSERT_ID()` is ever a trigger's: an insert
into a table counting nothing whose trigger writes a counted one reports 0 and leaves
`LAST_INSERT_ID()` alone. A trigger's row sets off the trigger of the table it writes in turn,
each counted table taking its own numbers. A statement whose triggers would write a table it
writes or reads — a trigger writing its own table, or an `INSERT ... SELECT` reading a table a
trigger down the line writes — is refused, where MySQL answers 1442. A trigger naming the id it
writes into a counted table and one writing a `BIGINT UNSIGNED` counted table are refused; see
TODO.md for these and for a trigger's row that breaks `NOT NULL`, which spends a number here
and not in MySQL.

An `UPDATE` or `DELETE` can name the rows it touches. It could not before: the
`WHERE` of a DML statement took `AND`, `OR`, `NOT`, `IS NULL` and a boolean
literal but no comparison at all, so `UPDATE t SET a = 1 WHERE id = 1` answered
1235 while `UPDATE t SET a = 1` with no `WHERE` succeeded and changed every row.
That is the wrong way round for a client to be told no.

A comparison there now goes through the same checked path a `SELECT` comparison
goes through, and is held to the same rule, so the rows a `WHERE` names cannot
depend on which statement is asking. `WHERE id = 1`, `WHERE 1 = id`, `WHERE name = 'z'`,
`WHERE name LIKE 'z%'` and `WHERE id BETWEEN 1 AND 10` all run, the text ones ignoring
case as described above. The chained, `IN` and `<=>` forms stay refused for the same
reason they are in a `SELECT`.

An `UPDATE` or `DELETE` can also order and limit the rows it affects with `ORDER BY col [ASC|DESC]`
and `LIMIT n`. Because Turso's core SQLite parser does not enable `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`,
the statement is translated into a subquery filtering rows by `_rowid_ IN (SELECT _rowid_ FROM <table> [WHERE ...] ORDER BY ... [LIMIT ...])`.
Ordering requires an integer column family (refusing text columns where collation semantics cannot yet
be preserved in DML rendering) and non-ordinal column identifiers. A `LIMIT` without an `ORDER BY` is
refused as non-deterministic.

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
Both `SHOW TABLES LIKE 'pattern'` and `SHOW FULL TABLES LIKE 'pattern'` match
case-insensitively using `MySqlLikePattern`, naming the primary column
`Tables_in_<db> (<pattern>)`. Pattern filtering occurs after scanning the
catalog and checking authorization, preserving `TABLE_LIST_SCAN_LIMIT` truncation
safety so an oversized schema fails closed rather than producing a truncated
list that hides existing tables.

Rather than print DDL that leaves something out, these answer `1235`: a table
carrying an index, a `CHECK` or `FOREIGN KEY` constraint, or a string DEFAULT
on an integer column. The constraints have no line to go on yet, and MySQL
escapes a string default the way its own parser reads it back, which is not
what this frontend stores. A `TEXT` or `BLOB` DEFAULT is the one thing dropped
silently, matching MySQL, which rejects those defaults outright with 1101.

`SELECT @@version`, `SELECT @@version_comment` and `SELECT VERSION()` are
answered from what this server is. The `mysql` client opens with
`select @@version_comment limit 1`, so the `LIMIT` MySQL takes there is read and
dropped — this answers one row, and a limit can only keep or discard it, though
`LIMIT 0` is refused rather than answered with a row it asked not to have. A
scope prefix and an alias are both taken, and the column is otherwise named
after the expression as written, which is `@@version` for `SELECT @@version`,
measured.

The version is the string the handshake announced, since a client compares the
two. `@@version_comment` says what this is rather than what MySQL's says.
Measured on 8.4.11: `@@version` is a `VAR_STRING` of length 87380 with no flags
and `decimals` 31, while `VERSION()` is a `VAR_STRING` of length 24 and is NOT
NULL — a call and a variable differ in both, and an alias over either leaves
that alone, measured. One reader answers every variable this server has an
answer for, a row of them at a time, and a name it does not know is refused
rather than answered with a value this server does not have.

A client's opening `SET` statements are taken when the server is already in the
state they ask for, and refused when they would change it. Every real client
sends a handful of these before any work starts, so refusing them all ends the
connection at hello; but taking one that would change how the server behaves is
worse, because the client goes on believing a setting took effect. So each is
checked against what this server actually does.

Four settings are read, in each of the spellings MySQL takes — `SET`,
`SET SESSION`, `SET LOCAL`, `@@name`, `@@session.name` — and inside the
versioned comment `mysqldump` wraps them in, since `/*!40100 SET @@SQL_MODE='' */`
runs on any server past the named version.

`SET NAMES` is taken for `utf8mb4`, with no collation or with
`utf8mb4_general_ci`, `utf8mb4_0900_ai_ci` or `utf8mb4_unicode_ci`, and so is
`SET collation_connection` naming one of those three. Laravel opens every
connection with `SET NAMES 'utf8mb4' COLLATE 'utf8mb4_unicode_ci'`. The
connection starts on the collation the client's handshake names when it is one
of those three, as in MySQL, and on `utf8mb4_general_ci`, the collation this
server's greeting names, for any other utf8mb4 collation and for an ID MySQL
has no collation for, where MySQL gives the session `utf8mb4_0900_ai_ci`. A
`SET NAMES` without a collation goes back to `utf8mb4_general_ci`, where
MySQL 8.4 would go to `utf8mb4_0900_ai_ci`. Any other character set or
collation is refused, but for latin1 below.

`SET character_set_client`, `character_set_results` and `collation_connection`
also take `latin1` and `latin1_swedish_ci`, which is what a dump sets around
every view whose creator's client was left at its default — measured, the
`mysql` client in the 8.4.11 image is. latin1's first 128 characters are
ASCII's, as utf8mb4's are, so while the session names latin1 for what it sends
or for its connection a statement written in ASCII is taken and one with any
other byte is refused, since latin1 reads that byte as another character; the
view text a dump writes is ASCII. While it names latin1 for its results, a
result whose text columns hold only ASCII, and whose columns are named in
ASCII, is sent as it is, being the same bytes in latin1; one holding any other
character is refused rather than sent in utf8mb4. Measured on 8.4.11 over a
latin1 connection, MySQL describes each text column of such a result as
latin1_swedish_ci (8) with its length counted in latin1's one-byte characters
— `select @@version_comment limit 1`, which the `mysql` client sends as it
starts, is 21845 where utf8mb4 makes it 87380, `VERSION()` 6 where utf8mb4
makes it 24, a `VARCHAR(20)` 20 and a `TEXT` 65535 — and leaves a number's
binary character set alone, and this describes them the same. A binary string
is sent as it is by MySQL too, whatever it holds. A prepared statement is
refused while any of the three names latin1. Each reads back as
named, and `SET NAMES utf8mb4` or the dump's own restoring `SET` ends it. A
view made in that window is kept with no record of the latin1: MySQL's
`information_schema.VIEWS` reports `latin1` and `latin1_swedish_ci` for it,
and this server's reports utf8mb4.

Measured on MySQL 8.4.11: the connection's collation is what every text column
of a result reports — 224 under `utf8mb4_unicode_ci`, 255 under
`utf8mb4_0900_ai_ci` — whatever the column is declared with, and that holds
here for text and prepared results alike. It decides nothing else here: a
column is compared under its own collation, as in MySQL, and the one place the
connection's collation would decide a comparison, two written words compared
with each other, is refused.

`SET sql_mode` is taken when every mode it names is one this server already
behaves as. `ANSI_QUOTES` and `NO_BACKSLASH_ESCAPES` have to match the session,
because they change what a double quote and a backslash mean. The rest of MySQL
8.4's default `sql_mode` describes behavior this already has, so a client that
reads the variable and writes it back is taken: `STRICT_TRANS_TABLES` and
`STRICT_ALL_TABLES` because writes are refused rather than truncated,
`NO_ZERO_IN_DATE` and `NO_ZERO_DATE` because an impossible date is refused,
`NO_ENGINE_SUBSTITUTION` because `InnoDB` is the only engine and is what
`SHOW CREATE TABLE` reports, `ONLY_FULL_GROUP_BY` because `GROUP BY` enforces
it, and `ERROR_FOR_DIVISION_BY_ZERO` because division never reaches a write.
`NO_AUTO_VALUE_ON_ZERO` changes what this server does, and is kept: measured
on 8.4.11, a 0 written into a counted column is stored as 0 under it — a second
one is a duplicate key — while NULL still takes the next number; without it, a
0 takes the next number as NULL does. Every other mode is refused rather than
quietly ignored.

The value may be worked out from the one in force, which is how Rails opens
every connection: `CONCAT(@@sql_mode, ',STRICT_ALL_TABLES')`, `REPLACE` over
it, `@@GLOBAL.sql_mode` or `DEFAULT` for what a new session starts with, and
any nesting of those. Measured on 8.4.11, a `REPLACE` of text that is not
there leaves the value alone and the empty entries it leaves are dropped.

One `SET` may make several assignments separated by commas — Rails sends
`SET NAMES utf8mb4, @@SESSION.sql_mode = ..., @@SESSION.wait_timeout = ...` —
and, measured on 8.4.11, when one of them fails none takes effect, which is
what happens here. A user variable may be set beside the settings, which is how
a standard `mysqldump` saves each one it changes —
`SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0` — and every value is
read before any is assigned: measured, `SET @a = '+01:00', time_zone = @a`
finds the `@a` in force before the statement. `time_zone`,
`foreign_key_checks`, `unique_checks`, `sql_notes`, `sql_mode` and the three
character-set names each take a user variable's value, which is how the dump
puts them back; a switch takes a variable holding 0 or 1 and a zone one holding
a word, and anything else is refused where MySQL answers 1231. A scope word holds for the assignments after it until
another is written, so `SET GLOBAL a = 0, b = 0` sets both globally there;
nothing here can change another session, so a global assignment, written as a
word or as `@@GLOBAL.`, is refused.

`SET wait_timeout` takes one second through a year, the range measured on
8.4.11, and the connection keeps that idle time in place of the server's own
until the session sets `DEFAULT` or resets. MySQL clamps a value past either
end with a warning where this refuses it. `SET sql_auto_is_null = 0` and `SET
sql_safe_updates = 0` are taken, being what this server does; 1 asks for a
rule it does not have and is refused.

`SET time_zone` takes `UTC`, `SYSTEM` and fixed offsets from `-13:59` through
`+14:00`, the limits measured on MySQL 8.4.11. A `TIMESTAMP` value written by
an explicit `INSERT ... VALUES` is converted from the session offset to UTC;
a direct result column is converted back to that offset. The restricted query
and write shapes are described with `TIMESTAMP` below.
`SET information_schema_stats_expiry` is taken for any value: it is how long
MySQL caches `information_schema` statistics, and there are none here.

`SET max_execution_time` takes any whole number of milliseconds and `DEFAULT`,
which is 0 and no limit, and `@@max_execution_time` and `SHOW VARIABLES` read
it back as MySQL does, an unsigned `LONGLONG` of 21. A `SELECT` — text or
prepared, locking its rows or not — running longer is stopped with 3024, and
the shorter of this and the server's own query timeout holds. Measured on
8.4.11: a `SELECT` and a `SELECT ... FOR UPDATE` over a join too wide to finish
in 10 ms both answer 3024, and an `UPDATE` reading the same join runs on, which
is what happens here. MySQL clamps a negative value to 0 with warning 1292 and
answers 1232 for a word, where this refuses both.

`SET sql_mode = 'TRADITIONAL'` is taken, alone or beside other modes: measured
on 8.4.11 it stands for `STRICT_TRANS_TABLES`, `STRICT_ALL_TABLES`,
`NO_ZERO_IN_DATE`, `NO_ZERO_DATE`, `ERROR_FOR_DIVISION_BY_ZERO` and
`NO_ENGINE_SUBSTITUTION`, each a mode this server behaves as, and MySQL keeps
`TRADITIONAL` as a mode of its own and reads it back before
`NO_ENGINE_SUBSTITUTION`. `ANSI` stands for modes this server does not keep and
is refused. `@@sql_mode` reads `ONLY_FULL_GROUP_BY` back whatever a session
names, the rule being one this server keeps regardless; MySQL drops it when a
session leaves it out.

`SHOW COLLATION` and `SHOW CHARACTER SET` (or `SHOW CHARSET`) list what this
server has rather than everything MySQL has: the utf8mb4 collations a column,
a table or the connection may be declared with — `utf8mb4_0900_ai_ci`,
`utf8mb4_bin`, `utf8mb4_unicode_ci` and the handshake's `utf8mb4_general_ci`
— and the `binary` collation and character set every `BLOB` holds, where MySQL
lists 287 collations over 41 character sets. Each row and each column's shape
was measured on 8.4.11, and both come back in name order, as MySQL answers
them. No database need be selected. A `LIKE` names the rows to list and a
`WHERE` may test the columns with `=` and `LIKE` joined by `AND`: measured,
both match words without regard to case, so `Charset = 'UTF8MB4'` lists the
utf8mb4 rows; a column the listing has not got is 1054, and any other test is
refused.

`SHOW PROCESSLIST` and `SHOW STATUS` stay refused. Measured on 8.4.11, a
session without the `PROCESS` privilege sees every connection of its own
account, a pool's other connections among them, and a session here knows only
itself; and the 330 status counters describe the whole server.
`group_concat_max_len` is a session setting here as it is in MySQL; see
"`GROUP_CONCAT` and `group_concat_max_len`".

A column may name a `CHARACTER SET` or a `COLLATE`, which a dumped schema
spells out on every text column, so refusing them stopped a `mysqldump` from
being restored. `utf8mb4` is the accepted character set. Text columns take
`utf8mb4_0900_ai_ci` by default and may explicitly name `utf8mb4_bin`, which
uses its own PAD SPACE byte collation, or `utf8mb4_unicode_ci`. Other names are
refused rather than ignored.
With `character_set_results=utf8mb4`, a selected text column reports the
connection's collation ID on the wire — 45 until the session names another —
including a `utf8mb4_bin` column.
With `SET character_set_results = NULL`, the original column IDs are reported:
255 for `utf8mb4_0900_ai_ci`, 46 for `utf8mb4_bin` and 224 for
`utf8mb4_unicode_ci`, as measured on MySQL 8.4.

The accepted collation is stored in the engine schema. `SHOW FULL COLUMNS` and
`information_schema.COLUMNS.COLLATION_NAME` report it. `SHOW CREATE TABLE`
prints a column's collation the way MySQL 8.4.11 does, measured: a column
whose collation differs from its table's gets ` CHARACTER SET utf8mb4 COLLATE
<name>`, one that takes a table collation other than `utf8mb4_0900_ai_ci` gets
` COLLATE <name>`, and one that takes `utf8mb4_0900_ai_ci` from its table gets
nothing. MySQL also prints the longer form for a column that named its table's
own collation itself; this does not remember which columns did, so such a
column gets the shorter form, or nothing in a `utf8mb4_0900_ai_ci` table.

`utf8mb4_unicode_ci` is the collation Laravel and Prisma declare every table
with. It is MySQL's Unicode 4.0.0 collation, and its weights here are
generated from Unicode's own `allkeys-4.0.0.txt` — checked against MySQL
8.4.11's `WEIGHT_STRING()` for every character of the Basic Multilingual
Plane, with no difference. Measured and matched: it ignores case and accents
(`'á' = 'A'`, `'ß' = 'ss'`); it pads with spaces, so `'a' = 'a '` where
`utf8mb4_0900_ai_ci` tells them apart, and a tab, weighing less than a space,
sorts `'a\t'` before `'a'`; about 470 control and formatting characters weigh
nothing, so `'a' = CONCAT('a', CHAR(1))`; a character Unicode 4.0.0 had not
yet assigned — `ₐ`, U+2090 — has a weight of its own rather than the `a` it
matches under `utf8mb4_0900_ai_ci`; and every character past the Basic
Multilingual Plane weighs the same, so all emoji are equal to one another. Keys
and `UNIQUE` constraints compare the same way: a `'A'` or an `'a '` is a
duplicate of an `'a'`.

`LIKE` over a `utf8mb4_unicode_ci` column matches one character at a time
under those weights, as MySQL does: `'á' LIKE 'A'` and `'ß' LIKE '_'` match,
while `'ß' LIKE 'ss'`, `'a ' LIKE 'a'` and two different emoji do not. The
engine picks the matcher from the collation of the column being matched, so a
query reading a `utf8mb4_unicode_ci` column and a `utf8mb4_0900_ai_ci` column
matches each under its own. `FIELD`, `GREATEST`, `LEAST` and `NULLIF` over a
`utf8mb4_unicode_ci` or `utf8mb4_bin` column are refused, and so is ordering
by a call over one that answers text — `ORDER BY LOWER(name)`, `ORDER BY
CONCAT(name, 'x')` — or comparing such a call with written text, in a `SELECT`,
an `UPDATE` or a `DELETE`: each compares under `utf8mb4_0900_ai_ci`'s weights,
where MySQL compares a call's answer under the collation of the column it
read.

A table takes `COLLATE=utf8mb4_unicode_ci` in each of MySQL's spellings —
`DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci`, `COLLATE=...`,
`COLLATE '...'`. Measured on MySQL 8.4.11, every text column of the table that
names neither a character set nor a collation takes the table's, and so does one
an `ALTER TABLE ... ADD`, `MODIFY` or `CHANGE` writes the same way; a column
naming only `CHARACTER SET utf8mb4` takes `utf8mb4_0900_ai_ci`, that character
set's own default. The table's collation is kept with its stored definition, so
`SHOW CREATE TABLE` ends with `DEFAULT CHARSET=utf8mb4
COLLATE=utf8mb4_unicode_ci`, and `information_schema.TABLES.TABLE_COLLATION`
and `SHOW TABLE STATUS` report it.

`VARBINARY(n)` is taken. It holds bytes rather than characters, which is the
whole of the difference from a `VARCHAR`: measured on 8.4.11, a
`VARBINARY(255)` reports VAR_STRING with length 255 — the declared count itself,
not four bytes reserved for each of them — the binary collation, and the BINARY
flag. `SHOW CREATE TABLE` and `SHOW COLUMNS` print `varbinary(255)`, and a value
longer than the declared count is refused the way an over-long `VARCHAR` is,
counting bytes.

`BINARY(n)` is refused. Measured on the same server, it pads a shorter value
with NUL bytes to the declared width — `'ab'` in a `BINARY(16)` reads back
sixteen bytes long — and the engine has no padding, so taking it would store a
different value than MySQL stores and answer a different length for it.

A `FOREIGN KEY` is refused, and refused at the door rather than taken and left
to do nothing. The parser can translate one; the frontend is what says no. The
engine runs with `PRAGMA foreign_keys` off, so a constraint accepted here would
not be enforced, where MySQL answers 1452 for a child row whose parent does not
exist — measured on 8.4.11. A client that wrote the constraint would be
reasoning about integrity it does not have, which is the same reason a lock is
never handed out here unless it is really held.

The inline column spelling, `parent_id INT REFERENCES parent(id)`, is refused
too, and that one is a divergence rather than a gap: measured on 8.4.11, MySQL
parses it and ignores it — `SHOW CREATE TABLE` shows no key and a row with no
parent inserts — so MySQL takes a schema here that this refuses.

`SELECT ... FOR UPDATE` and `SELECT ... FOR SHARE` are taken, and the lock they ask for is
really held. The engine holds one write lock over the whole database and takes it when a
statement writes, so the statement that asks for the lock takes it by writing no row — an
`UPDATE` that sets a column to itself where nothing matches. Another session that tries to
write while it is held answers 1205, which is what MySQL answers when a lock wait runs out.

A session kept out by that lock **waits**, the way MySQL's does, and answers 1205 once the
wait runs out. The wait starts at fifty seconds, which is where MySQL's
`innodb_lock_wait_timeout` starts, and `SET SESSION innodb_lock_wait_timeout = <n>` changes
it for the session — a whole number of seconds from one to 1073741824, the range MySQL takes.
So the read-then-write pattern behaves the way it does against MySQL: the second session
blocks until the first commits, and then goes on.

One thing still differs, and it is one fact: the lock is one lock over the whole database
rather than a lock for each row. It is **stronger** than the one MySQL takes, so a session is
kept out of every table rather than out of the rows that were read — two sessions locking
unrelated rows are serialized here where MySQL would let both through.

Outside a transaction no lock is taken. MySQL's would end with the statement that took it, so
there is nothing to hold.

`SKIP LOCKED` — Laravel's database queue takes its next job with `FOR UPDATE SKIP LOCKED` — asks
for the rows no other session holds, and takes the same lock. Once this session holds the one
lock, no other session holds any row, so the rows it answers are ones no other session holds, as
MySQL's are; but it **waits** for the lock where MySQL skips the rows another session holds and
answers at once, and answers 1205 once the wait runs out. `NOWAIT` asks to be refused when a row
it reads is held, which one lock over the whole database cannot tell, and `OF <table>` names
which tables to lock; both are refused.

`LOCK IN SHARE MODE`, MySQL's older spelling of `FOR SHARE` — the one Laravel's
`sharedLock()` and Rails' `lock("LOCK IN SHARE MODE")` write — is read as `FOR SHARE` and takes
the same lock. Measured on 8.4.11, the two are read alike wherever a locking clause may stand:
after `ORDER BY ... LIMIT`, in a subquery, after a `UNION`, in any case and with a comment
between its words — so here it is read as `FOR SHARE` wherever it stands, and is taken or
refused exactly where `FOR SHARE` is. The older spelling takes none of `NOWAIT`, `SKIP LOCKED` or `OF <table>`,
and cannot be followed by a `LIMIT`; MySQL answers 1064 for each, and so does the parser here.

`GET_LOCK`, `RELEASE_LOCK`, `IS_FREE_LOCK` and `RELEASE_ALL_LOCKS` work on MySQL's named
locks, the ones Rails, Prisma and Flyway each take around a migration. They belong to no
database and no transaction: every session of the server shares one table of them, a
rollback leaves a lock held, and a session lets go of all it holds when it resets or ends.
Measured on 8.4.11 and matched: names are matched whatever their case; a session may take a
lock it holds again and must let it go as many times, which `RELEASE_ALL_LOCKS` counts; a
`NULL` timeout waits no time and a negative one waits without end; `RELEASE_LOCK` answers 0
for a lock another session holds and NULL for one no session does; an empty or `NULL` name is
3057 and one past 64 characters 4163; and a session that would wait for a lock held by a
session already waiting on it is told 3058 at once and keeps the locks it has. The calls are
answered in a `SELECT` of them alone, with the result columns MySQL reports. `IS_USED_LOCK`
is refused, since it answers the holder's connection ID and this server hands none out.

`LOCK TABLES` takes a lock and holds it until `UNLOCK TABLES`, which is what the statement
asks to be true. It was refused while there was no way to hold one; there is now, and it is
the same one `SELECT ... FOR UPDATE` takes — the engine's write lock, held for as long as the
write transaction the statement opens stays open. A session that writes while it is held
waits and answers 1205. MySQL commits an open transaction before it locks, and so does this,
because a write transaction cannot be opened inside another.

It locks **more** than was asked for: one lock over the whole database rather than a lock for
each table named, so the tables in the statement are read and then let go — locking any of
them locks all of them. `READ` takes the same lock as `WRITE` for the same reason there is no
weaker lock to take under `FOR SHARE`. Two things follow, and both are written here rather
than left to be found. MySQL lets the locking session touch only the tables it locked and
answers 1100 for the rest; this lets it touch any of them, which is more permissive rather
than a promise broken. And the statements between the two are inside one transaction, so they
commit together at `UNLOCK TABLES`, where MySQL commits each on its own — which is why
`START TRANSACTION`, `COMMIT` and `ROLLBACK` are refused while the lock is held: each would
end the transaction holding it, and letting go of a lock the client believes it holds is the
quiet lie this was refused for in the first place.

`READ LOCAL` is refused, because it lets other sessions insert while the lock is held and one
write lock cannot. So is `LOW_PRIORITY WRITE`, which changes who waits for whom, and
`LOCK INSTANCE FOR BACKUP`. An `UNLOCK TABLES` holding nothing answers OK, the way MySQL's
does.

`ALTER TABLE` takes several operations in one statement, which is how a
migration writes one. The engine takes one operation per statement, so the
statement is split into one per operation and they run inside a transaction:
measured on 8.4.11, `ADD COLUMN c, ADD COLUMN a` against a table that already
has `a` answers 1060 and adds neither, so a failure part-way has to leave the
table as it was. The pieces are rendered back as MySQL rather than handed on as
SQLite, so each goes through the ordinary schema path — the one that carries the
durable DDL a table is remembered by, and the checks an `ALTER` has to pass
against a marked view or trigger. An operation outside the checked set is
refused before any of them runs.

Supported column and index operations may appear in the same statement. The
child-index requirement for a foreign key is checked after all operations, so
`DROP INDEX old_key, ADD INDEX new_key (...)` can replace its supporting index
without leaving the table in an invalid state. If the replacement fails or the
final schema has no covering index, the transaction restores the columns and
indexes as they were. This is tested across a reopen as well as in memory.

A table that counts its own ids takes an `ALTER` as well, and goes on counting.
Its key is a rowid alias in the engine — `id INTEGER PRIMARY KEY`, carrying no
`NOT NULL` of its own — so writing the table back out the ordinary way would
produce a `CREATE TABLE` this frontend refuses to read. The renderer is told
which column the table counts on and writes that one the way it was declared,
which is what lets the rewrite happen at all; the marker saying the table is
counted rides along with it, and the counter is left where it stood. Measured on
8.4.11 and matched: after an `ADD COLUMN` the printed schema still carries
`AUTO_INCREMENT` on the key and `AUTO_INCREMENT=3`, the next counted row is 3,
and dropping an ordinary column or renaming the table changes neither.

Taking the counted column away is refused — `DROP COLUMN id`, a `RENAME COLUMN`
of it, and a `MODIFY COLUMN` of it — because what would be written back is a
table counting on a column that is not there. MySQL takes the drop and leaves an
ordinary table behind, which is the difference: a gap rather than a different
answer, and the refusal leaves the table as it stood.

`CREATE TABLE ... AS SELECT` makes a table out of what a `SELECT` answers.
MySQL works the new columns out from the result, so this reads them out of the
source table's stored DDL, writes a `CREATE TABLE` of its own, and fills it with
an `INSERT`; both run inside one transaction, so a failure never leaves an empty
table behind. Measured on 8.4.11: the copy keeps each column's type, its `NOT
NULL` and its `DEFAULT`, and loses the keys and the `AUTO_INCREMENT`. What takes
a dropped `AUTO_INCREMENT`'s place is a zero default — `id int NOT NULL
AUTO_INCREMENT PRIMARY KEY` copies as `id int NOT NULL DEFAULT '0'`, where a
plain `a int NOT NULL` copies with no default at all. `ROW_COUNT()` afterwards is
the number of rows copied, and a `ROLLBACK` after one leaves the table there.
An explicit `utf8mb4_bin` or `utf8mb4_unicode_ci` collation on a copied text
column is retained.

Every projected item has to be a plain column or integer arithmetic over
columns, with or without an alias, or a lone `*`; an alias renames the column in
the copy. Arithmetic makes a `BIGINT` — measured, `a + 1` is `bigint NOT NULL
DEFAULT '0'` where `a` is NOT NULL, `a + b` over a nullable `b` is `bigint
DEFAULT NULL`, and nesting changes neither: `a + 1 - 2` and `a * a` are the
first. Division is refused with them, making a `decimal(14,4)` on a rule of its
own, and so is an unaliased expression column, whose name MySQL takes from the
expression's own text — measured, `SELECT a + 1` makes a column called `a + 1`.
A source column with a string `DEFAULT` is refused as well, for
the reason `SHOW CREATE TABLE` refuses to print one: the escaping is not decided
here. So are declared columns beside the `SELECT`, `IF NOT EXISTS`, `TEMPORARY`,
and a `SELECT` this frontend does not already take.

`TRUNCATE TABLE` empties a table. The engine has no `TRUNCATE`, so an
unfiltered `DELETE` does the emptying, wrapped in the commits MySQL's DDL makes
around it. Measured on 8.4.11: `ROW_COUNT()` after one is 0 whatever the table
held, and a `ROLLBACK` after one leaves the table empty and commits the write
that came before it, so the statement is bracketed by a commit on each side
rather than run inside whatever transaction was open. The `TABLE` keyword is
optional, which MySQL takes too, and an unknown name and a view both answer 1146
— measured, MySQL says the view's own name does not exist.

A table that counts its own ids is taken, and its counter starts again the way
MySQL's does. The durable allocator only ever moves its high water forward, so
there is no winding it back; what MySQL's own `TRUNCATE` does is drop the table
and make it again, and that is what happens here. The table is written again
from what it was stored as — taking a fresh allocator identity, which counts
from 1 the way a new table's does — and its indexes are written again beside it
from what they were stored as. Measured on 8.4.11 and matched: the table is
empty, `SHOW CREATE TABLE` prints no counter at all, its indexes and foreign
keys are still there, and the next row takes 1.

A table carrying a trigger is refused there. A trigger is not the table's own
row and would not come back with it, where MySQL leaves one where it stood.

A table another table's foreign key names is refused while foreign key checks are
on, which is what MySQL does: measured on 8.4.11, it answers 1701, SQLSTATE
42000, and names the child table and the constraint. The message here stays
fixed, as every other one does.

With the checks off the statement goes ahead and leaves the child rows pointing
at nothing — measured, the parent comes back empty and the child keeps its row.
That is what a test suite's teardown asks for: it turns the checks off exactly so
that every table can be emptied in turn without their order mattering, and
turning them back on leaves the rows where they are rather than looking at them
again.

`ALTER TABLE ... ADD COLUMN ... FIRST` and `... AFTER x` say where the column
goes, and a migration written for MySQL says so often. The engine puts a new
column last and cannot move one, and a table's column order is something a
client sees — in `SELECT *`, in `SHOW CREATE TABLE`, and in an `INSERT` naming
no columns — so the table is written again with the column standing where the
statement asked. That is what MySQL's own `ALTER TABLE` does: the old table is
set aside under a name of its own, the new one is made under the name they
share, the rows are carried across, the old one is dropped, and its indexes are
written again after that.

The rows go across through the engine rather than through the frontend's own
`INSERT`, which would refuse to write a counted column its numbers; they are the
rows the table already has, with the numbers they already carry. Foreign key
checks are off while it runs, every row being carried across, so nothing a key
names goes missing. A table that counts its own ids takes an allocator of its
own when it is made again, so its counter is raised afterwards to where it stood.

Measured on 8.4.11 and matched: the column stands where it was asked for, every
row already there takes the column's own default, the table's indexes and its
counter are exactly as they were, and the next counted row takes the number it
would have taken. `AFTER` naming the last column is the place the column takes
anyway, so the statement runs as the ordinary one it means; `AFTER` naming a
column the table has not got answers 1054, which is what MySQL answers. A table
carrying a trigger is refused, a trigger not being the table's own row.
Moving a column in a foreign-key child table retains its constraint and
supporting indexes through the rewrite and a reopen. A table another table's
foreign key names is written again the other way round, since the engine
points a child's key at a renamed table's new name: the new table is made
under a name of its own, the rows are copied into it, the old one is dropped,
and the new one takes the name the child's key still names. Laravel adds its
`->after()` columns to `users` and `posts`, which `posts` and `post_tag` name;
measured on 8.4.11 and matched, the rows keep their values and take the new
column's default, a child row naming no parent is still refused, a delete
still cascades, and the counter goes on from where it stood, across a reopen.

Laravel writes every `->after()` of one migration into one `ALTER TABLE` —
`add balance ... after password, add is_active ... after balance, add profile
... after is_active`. Measured on 8.4.11 and matched: the clauses go in the
order written, each placed in the table the ones before it left, so `ADD a
AFTER x, ADD b AFTER a` leaves `x, a, b` and `ADD h AFTER y, ADD i AFTER y`
leaves `y, i, h`; a clause naming no place puts its column last at its turn;
and `AFTER` a column only a later clause adds is 1054, leaving the table as it
was. Such a statement is taken when every clause adds a column; one mixing a
placed `ADD` with any other operation is refused. A rewrite of any table is
refused in a database holding a view or a trigger written through this
server, the engine refusing to rename a table there.

A table's stored definition is held to being exactly what the reader that
canonicalises one would write, which is how a tampered schema row is caught, and
that means every renderer of one has to write the same bytes. Two of them did
not: the reader wrote a foreign key's columns as `` REFERENCES `p`(`id`) `` and
the rewrite an `ALTER TABLE` goes through wrote the same thing with a space
before them. A counted table carrying a foreign key was therefore readable until
an `ALTER TABLE` touched it, and unreadable after — `SHOW CREATE TABLE`, `SHOW
COLUMNS` and `information_schema.COLUMNS` all refusing it. The rewrite writes it
without the space now, which is what was already stored, so nothing written
before changes. What `SHOW CREATE TABLE` prints is a renderer of its own and
still writes MySQL's spacing.

`MODIFY` and `CHANGE` take a place too, and are written again the same way. The
difference is that the column is already there, so it leaves the column list
before the place is counted — measured, `MODIFY n ... AFTER s` over (id, n, s)
leaves (id, s, n) — and its values come across under whatever name it ends up
with, which is how a `CHANGE` that renames it is answered. Measured on 8.4.11 and
matched: the column holds the values it held, an attribute the statement does not
restate is dropped as it is without a place, the table's indexes and counter are
as they were, and a `CHANGE` onto a name the table already carries answers 1060.

Moving the column a table counts on, or the one its key is over, is refused: the
counted column stands for the engine's rowid and the key is what the rows are
found by, and neither survives being written somewhere else.

`ALTER TABLE` also adds and drops indexes, which is the other half of what a
migration writes. The engine has no `ALTER TABLE ADD INDEX`, so each operation
becomes a `CREATE INDEX` or a `DROP INDEX` of its own and they run inside one
transaction, the way the inline `KEY` clauses do. `ADD INDEX`, `ADD KEY` and
`ADD UNIQUE INDEX` are taken, and `DROP INDEX`. Measured on 8.4.11: the result
of the three adds prints back byte for byte as MySQL's own `SHOW CREATE TABLE`,
1061 answers a name the table already carries and 1091 one it does not.

`information_schema.TABLES` is a table the engine scans, so a query over it
goes through the ordinary `SELECT` path: it may filter and order by any column
it names, which no amount of recognizing written shapes could give. What a
session may see is decided the same way it always was — a database-wide grant,
or the grants it holds on the tables the rows would name — and the answer is
left on the connection for the table to read, because the table is registered
on the database rather than on one session. A prepared query over it binds its
values the way a text one writes them — GORM's Migrator lists a database's
tables with `where TABLE_SCHEMA=?` and a table's indexes from `STATISTICS` with
the database and table names bound — its values held to the types the table
declares for its columns.

A wildcard over `TABLES` is refused: it asks for MySQL's twenty-one columns
and this answers eight, so a row of a different width would come back. So is a
call over one of
these columns, whose shape has not been measured; counting them works, since a
count does not depend on what a column holds. `CONCAT` over the words of any
`information_schema` table is the exception, which is how TypeORM's
`clearDatabase` writes out the statements dropping each view —
``concat('DROP VIEW IF EXISTS `', table_schema, '`.`', table_name, '`')`` over
`VIEWS`: measured on 8.4.11, it answers a nullable `VAR_STRING` as wide as each
word and column counted four bytes to a character, 612 there, with 31 decimals
and the binary flag the catalog's words carry. Over a column of another kind —
`VIEW_DEFINITION`, a `LONGTEXT` answered as a `MEDIUM_BLOB` — it stays refused. The sum of the two storage
counters — Laravel lists tables with `(data_length + index_length) as size` — is
answered with the shape measured on 8.4.11, an unsigned `LONGLONG` of length 22
without the binary flag; every other arithmetic over these columns is refused.

A `CASE` whose branches are one `information_schema` column or NULL is taken
too. Django's `get_table_description`, which `inspectdb` and every migration
altering a column run, reads `CASE WHEN collation_name = 'utf8mb4_0900_ai_ci'
THEN NULL ELSE collation_name END` out of `COLUMNS`. Measured on 8.4.11 over
`COLLATION_NAME`, `DATA_TYPE`, `COLUMN_COMMENT` and `NUMERIC_PRECISION`: the
answer reports the column's own shape — its type, length, collation and flags
— naming no table, and a NULL branch or a missing `ELSE` takes the `NOT_NULL`
flag off it. A catalog column beside a written value or another column, or
under `IFNULL` or `COALESCE`, stays refused.

`DATABASE()` and `SCHEMA()` after the `FROM` of a query reading
`information_schema` are the selected database's name, or NULL when none is
selected, which is what they answer; this is how Rails, Django and Laravel
each filter on the database they are in. `EXISTS (subquery)` is answered as a
result column too — Laravel's `hasTable` asks exactly that — with the shape
measured on 8.4.11, a NOT NULL `LONGLONG` of length 1 carrying the binary and
numeric flags; and `table_name IN (SELECT table_name FROM
information_schema.tables ...)`, which Rails writes, compares text with text.

A `SELECT`, `INSERT`, `UPDATE` or `DELETE` may write the selected database
before a table it reads or writes and before a column — Prisma writes
`prisma.users.id` for every column and TypeORM reads `typeorm.migrations`.
Measured on MySQL 8.4.11, those answer the rows and the column metadata the
bare names do, so the database is left out before the statement is read, in
text and prepared statements alike, matched without regard to case as
`lower_case_table_names=1` has MySQL do. Two shapes are refused rather than
read: an expression projected without an alias that holds such a name,
because MySQL names the result column after the text as written —
`SELECT COUNT(probe.users.id)` answers a column named `COUNT(probe.users.id)`
— and any other database, which the engine does not read from the selected
database's connection.

Prisma also writes each side of every join and of every key it compares in
parentheses — `ON (j2.id) = (posts.user_id)`, `WHERE (posts.id) = (?)`.
Measured on MySQL 8.4.11, those answer the rows and the columns the bare
comparison does, so a column, a value or a `?` in parentheses on either side of
a comparison is read as itself; parentheses around anything larger are kept as
the expression they hold.

Laravel's `Schema::getIndexes` and `Schema::getForeignKeys` — which `hasIndex`,
`db:table` and a migration's introspection read — each group a table's rows of
`STATISTICS`, or of `KEY_COLUMN_USAGE` joined to `REFERENTIAL_CONSTRAINTS`,
with `GROUP_CONCAT(... ORDER BY ...)`, which the statement path does not take.
Those two reads, exactly as Laravel 12 writes them with `schema()` or a named
database, are recognized and answered from the rows a plain read of the same
tables gives, over text and prepared. Measured on 8.4.11: one row for each index
or foreign key in the order of its name without regard to case, its columns
joined by commas in their order in the key; the joined columns a `LONG_BLOB` of
36864; `unique` a NOT NULL `LONG` of 1 over the text protocol and a `LONGLONG`
over the binary one; and each column read out of the catalog named after its
alias as its origin, in the view it was read through, over the text protocol,
and after the catalog table's own column over the binary one. The width of the
joined columns follows `group_concat_max_len` — 73728 under 2048 — by a rule not
worked out past that, so under any limit but MySQL's own 1024 the two reads are
refused.

Prisma's schema engine reads the catalog before `migrate deploy`, `db pull`,
`migrate diff` and `db push` with six statements the statement path does not
take: they compare and order names with `BINARY`, and join two catalog tables on
it. Those six, exactly as prisma-engines 7.1 (Prisma 6.19) writes them in
`sql-schema-describer`, prepared with the database bound, are recognized and
answered from plain reads of the same tables — the tables with a column,
their `CREATE_OPTIONS` (empty, as for every table this server takes) and
comments; every `CHECK` with its clause and its type lower-cased; every column
with an empty comment answered as NULL; every foreign key's columns joined to
its actions; every index's columns — each comparison and order Prisma writes
with `BINARY` made on the names' bytes, so the foreign-key read, which compares
the database's name that way too, finds nothing for a name written in another
case, as MySQL's does. Every column is described as MySQL
8.4.11 described it to `mysql_async` over the binary protocol: the name read
through `BINARY` a `VAR_STRING` of 192 in the binary character set, numbers
`LONGLONG` or `LONG` with MySQL's widths and flags, and each word a
`VAR_STRING`, `STRING` or `BLOB` of MySQL's width. Its two other reads, of
`VIEWS` and `ROUTINES`, are answered by the statement path as any other.
Measured, MySQL orders the columns read by their place in their table alone,
leaving columns of different tables at the same place in an order of its own;
this answers them in the order the catalog lists the tables, which Prisma, and
any reader keeping each table's columns in order, cannot tell apart.

`information_schema.STATISTICS` is the second such table, and the first this
frontend has ever answered. It reports one row per column of every index of
every table the session may see: the primary key first under the name
`PRIMARY`, then each index once per column it holds, with `NON_UNIQUE`,
`SEQ_IN_INDEX` and the `NULLABLE` of the column indexed — which is what a
migration tool reads to find out what indexes exist. All eighteen of MySQL's
columns are answered. `CARDINALITY` is always NULL: MySQL answers InnoDB's
estimate of the distinct values, cached for a day by default — measured on
8.4.11, 0 for a table that has since taken three rows, until the cache is let
go — and the engine keeps no estimate, so it answers NULL, which is what MySQL
answers for an index it has none for and what `SHOW INDEX` answers here. A
key column's `NULLABLE` is always empty, MySQL holding every key column NOT
NULL, and `SHOW INDEX` answers an empty `Null` for it likewise; measured on
8.4.11, both do for the counted key of an `AUTO_INCREMENT` table, which the
engine does not mark NOT NULL. A comparison against one of
these columns is held to the type the column holds rather than to text, so
`WHERE NON_UNIQUE = 0` is taken and `WHERE NON_UNIQUE = 'no'` is refused. An
`ORDER BY` over one of the text columns sorts without regard to case, the way
MySQL sorts them.

`information_schema.KEY_COLUMN_USAGE` is the third, and where a migration tool
reads foreign keys. It reports one row per column of every key that constrains
a value — the primary key, the unique keys and the foreign keys — and all
twelve of MySQL's columns. A plain index constrains nothing and has no row
here, which is what separates this table from `STATISTICS`. A foreign key names
the table and column it points at and its position in the key it references; a
primary or unique key leaves those four columns NULL, the way MySQL does. A key
written without a `CONSTRAINT` name is reported as `` `t_ibfk_1` ``, the same
name `SHOW CREATE TABLE` prints for it.

`information_schema.TABLE_CONSTRAINTS` and
`information_schema.REFERENTIAL_CONSTRAINTS` finish the set a migration tool
reads. The first is one row per constraint rather than one per column of one,
with `CONSTRAINT_TYPE` saying which of the three kinds it is. The second is one
row per foreign key, and the only place the `ON DELETE` and `ON UPDATE` a key
was written with are read back: measured on 8.4.11, a key written with no rule
reads back as `NO ACTION` on both, and `RESTRICT` reads back as written even
though neither is printed by `SHOW CREATE TABLE`. `UNIQUE_CONSTRAINT_NAME`
names the key in the parent the foreign key points at — `PRIMARY` for a primary
key and its own name for a unique one. Both answer all of MySQL's columns.

A `CHECK` constraint has a row in `TABLE_CONSTRAINTS` of type `CHECK`, and one
in `information_schema.CHECK_CONSTRAINTS`, read out of the stored DDL, each
under the name MySQL gives it. Measured on 8.4.11: a constraint written without
a name is `<table>_chk_<n>`, counted from one over the unnamed ones in the
order written, a column's own among them, and a named one takes no number.
The table is stored with its columns before its table-level constraints, so a
`CREATE TABLE` writing an unnamed table-level `CHECK` before a column carrying
an unnamed `CHECK` of its own is refused, MySQL numbering the two the other way
round. `CHECK_CLAUSE` is the expression the way MySQL writes it back —
measured, the whole in parentheses, a column in backquotes, each comparison
and each side of an `AND` or `OR` in parentheses of its own, the words in lower
case, a word with its character set in front and its quotes behind
backslashes, `(`s` <> _utf8mb4\'x\')`, and a negative number as `-(1)`: a
comparison, `IS [NOT] NULL`, `AND`, `OR`, `+`, `-` and `*` over columns,
numbers and words. A clause of any other form is refused when it is read,
its name still answered. A table carrying a `CHECK` is still refused by `SHOW
CREATE TABLE`.

`information_schema.COLUMNS` and `information_schema.SCHEMATA` are tables the
engine scans too, whose rows the session works out and leaves on the
connection before a statement that reads one runs: the columns are each
printed the way MySQL prints them, which takes the stored DDL and statements a
scan in the middle of a statement cannot run, and the databases live in the
server's catalog rather than in the database a connection holds. They are
worked out again for every statement, prepared ones included, so none reads
rows a schema change has made stale. The shapes the catalogue reader took
before still answer as they did; anything else — a wildcard, a join, a filter
or an ordering over any column — goes through the ordinary `SELECT` path.
`COLUMNS` answers all twenty-two of MySQL's columns. Measured on 8.4.11 over
one of every type: a `CHAR`, `VARCHAR`, `ENUM` or `SET` holds four bytes a
character in `CHARACTER_OCTET_LENGTH`, a `TEXT` or a binary type is measured
in bytes already so its two lengths are the same number, only a `TIME`,
`DATETIME` or `TIMESTAMP` has a `DATETIME_PRECISION` — a `DATE` has none —
and only a column holding words has a `CHARACTER_SET_NAME`, `utf8mb4`.
`PRIVILEGES` is what `SHOW FULL COLUMNS` answers for the same session. No
column here is generated or spatial, so `GENERATION_EXPRESSION` is empty and
`SRS_ID` NULL. A table whose columns this cannot read answers its name and no
more: a query about another table is answered, and one that reads that table's
columns is refused. The database name is compared as it was written —
measured, `TABLE_SCHEMA = 'TURSO_ORACLE'` answers nothing where
`'turso_oracle'` answers the columns — and a table or column name without
regard to case, since this frontend folds every table name it is given.
`SCHEMATA` answers the six columns MySQL has, for every database the session
may list, or the one it is in when it may list none; each is `utf8mb4` under
the collation it was made or last altered with, and none is encrypted. MySQL also lists
`information_schema`, `mysql`, `performance_schema` and `sys`, which `SHOW
DATABASES` here leaves out too. With no database selected, only the one
written shape the catalogue reader takes is answered, there being no
connection for the table to be scanned on.

`information_schema.ROUTINES` answers MySQL's thirty-one columns and never a
row: stored procedures and functions are refused here, so no database holds
one.

A wildcard is answered over every one of these tables but `TABLES`, each of
which answers all of MySQL's columns in the order MySQL declares them; `TABLES`
leaves out columns this server has nothing true to answer with.

`information_schema.VIEWS` answers MySQL's ten columns. Measured on 8.4.11,
`VIEW_DEFINITION` is the query as MySQL writes it back: every column in full
under the spelling its table stores and the name the view answers,
`` select `db`.`t`.`id` AS `ID` from `db`.`t` ``, and a view of written values
`` select 1 AS `one` ``. A view kept in the text MySQL prints — one with a
condition, grouping its rows, joining tables or reading a table under an
alias — is read back from that text with every table named in full and every
column of a table read under its own name too, `` `db`.`t`.`c` ``, where a
column of a table read under an alias keeps it, `` `p`.`c` ``: measured,
`` from (`db`.`users` `u` join `db`.`posts` `p` on((`p`.`user_id` = `u`.`id`))) ``.
`IS_UPDATABLE` is `YES` for a view over one table's columns, with a condition
or not, and for one joining tables' columns by inner joins, and `NO` for one
of written values, one grouping its rows and one with a `LEFT JOIN`. A view whose column its table no
longer has is refused when its definition is read, and one stored without the
account that made it is refused whenever `VIEWS` is read, as `SHOW CREATE VIEW`
refuses it.

A wildcard over one of them may stand in a derived table's body
too, whose columns are then that table's, and that body may be a `UNION` of
branches that each read the same columns of the same table — which is how
TypeORM's `loadTables` reads one table's rows a branch — as may a whole
statement. A `UNION` of three or more over anything else is still refused. MySQL reports an `information_schema` column's shape by how the
query is carried out — a query with an `ORDER BY` reports a column's original
table and flags differently from one without — and this answers the shape of
the query with one, as every column here was pinned to.

An `information_schema` query names the columns it wants, in the order it wants
them, and is answered that way. The catalog answers eight of MySQL's twenty-one
`TABLES` columns — `TABLE_SCHEMA`, `TABLE_NAME`, `TABLE_TYPE`, `ENGINE`,
`DATA_LENGTH`, `INDEX_LENGTH`, `TABLE_COLLATION` and `TABLE_COMMENT`. Measured on 8.4.11, a view has no engine,
collation or storage and its comment is `VIEW`; a table's engine is `InnoDB`,
its collation the one it was declared with and its comment the one it was
declared with, empty where it has none. `DATA_LENGTH` and
`INDEX_LENGTH` are figures InnoDB keeps and this server does not, so they are
NULL, as `SHOW TABLE STATUS` answers them. Which of those a query names, and in
what order, is up to the query, and the same column named twice is answered
twice, as MySQL answers it. A column outside the set is refused rather than
answered with a value that would be made up: `TABLE_ROWS` and the rest of the
table's statistics are numbers this server does not keep. `ORDER BY` may be
left off: the rows come back in table-name order, and a table's columns in
declaration order, whether or not the query asks for it.

`TRUE` and `FALSE` are the integers 1 and 0 in a comparison too, which is how
Active Record writes every boolean it compares — `WHERE users.admin = FALSE`,
`IN (TRUE, FALSE)` — so they meet an integer column as 1 and 0 do and a word
column as any number does, which is refused. An `UPDATE` may name a column it
sets qualified by the table it changes, `SET users.name = ...`, which is how
Active Record saves a loaded record; a qualifier naming any other table is
refused.

`ALTER TABLE` adds a foreign key to a table that already exists and takes one
away, which is the other half of what a migration writes. The engine had no
statement for either, so it has one now: `ADD CONSTRAINT` and `DROP CONSTRAINT`
are Turso's own, and they change the schema and nothing else — a table
constraint rewrites no row. The change is made on the SQL the table is
remembered by rather than on the table the engine built from it, because a
constraint's name lives only in the text.

The name is what makes the pair work, and the engine keeps it now: a foreign
key carries the name its `CONSTRAINT` clause wrote. `SHOW CREATE TABLE` prints
that name where there is one and MySQL's own `t_ibfk_N` where there is not, so
a named `CONSTRAINT` in a `CREATE TABLE` is taken as well — it was refused
before precisely because the name was dropped. `DROP FOREIGN KEY` and
`DROP CONSTRAINT` are MySQL's two spellings and both find the key by that name.
The key is enforced from the moment it is added: measured on 8.4.11, a child
row naming no parent answers 1452, and this answers the same.

A table that counts its own ids takes the key too. It did not, and that is the
one shape that mattered: every table an ORM writes counts its ids, and a
migration adds the key in a statement of its own once both tables are there. The
stored SQL was read back with the engine's own dialect rather than the
database's, and a counted table's stored MySQL DDL says `AUTO_INCREMENT`, which
that dialect does not know — so the statement was refused wherever the child
table counted. It is read through the database's dialect now, which is what
every other reading of stored schema SQL already used.

An `ADD FOREIGN KEY` written without a name is taken as well, and named the way
MySQL names one. Measured on 8.4.11: two unnamed keys added one after the other
read back as `t_ibfk_1` and `t_ibfk_2`, counting the keys the table already
carries, and this counts them the same way.

InnoDB creates a `KEY` beside the constraint — `KEY \`fk_b\` (\`b\`)` — and
keeps it after the constraint is dropped. The current frontend creates and
retains that supporting index as well.

`DROP KEY` is MySQL's other spelling for `DROP INDEX` and drops the same key.
The parser library reads only the second, so the words are swapped before it
sees them — on the tokens rather than on the text, so only the word right after
a `DROP` is read as the keyword and a column called `key` is left where it is.

An unnamed key takes its name from its first column, adding `_2`, `_3` and so
on when that name is taken. Supported index and column operations can share an
`ALTER TABLE` and apply together. The name `PRIMARY` remains refused for index
operations, since adding or dropping a primary key is a different operation.

MySQL prints `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci`
after every table, so that trailer ends every statement a dumped schema carries,
and this prints those same bytes whatever a table holds. A table written with
those options is therefore asking for the table it would get anyway, so they are
taken and left out — which is what lets a printed schema be handed straight back,
the way a test suite loads the schema it dumped.

Measured on 8.4.11 and matched: a table written with `ENGINE=InnoDB`, with
`DEFAULT CHARSET=utf8mb4`, `CHARSET=utf8mb4`, `CHARACTER SET utf8mb4` or
`DEFAULT CHARACTER SET utf8mb4`, or with `COLLATE=utf8mb4_0900_ai_ci` or
`DEFAULT COLLATE`, prints back byte for byte the same as one written with none,
and so does a table that counts its own ids. `COLLATE=utf8mb4_unicode_ci` is
taken too, and kept, as described with column collations. Anything else is a claim this cannot
keep and is refused rather than quietly dropped — measured, `DEFAULT
CHARSET=latin1`, `COLLATE=utf8mb4_bin`, `ENGINE=MyISAM` and `ROW_FORMAT=DYNAMIC`
are each printed back. The same option written twice is refused, MySQL taking
the last one written.

`AUTO_INCREMENT=<n>` says where a counted table's numbering starts, and it is the
option mysqldump writes on every table that has ever held a row, so a dumped
schema carries it wherever a counter does. It is taken: the table is created and
the allocator's mark is then raised so the first row takes the number the option
names. Measured on 8.4.11 and matched: the first row takes it, `SHOW CREATE
TABLE` prints the counter as it stands rather than as the statement wrote it — so
after that first row the printed number is one higher — 0 and 1 both leave the
counter where it already starts and print nothing, a table with no counted column
takes the option and prints nothing back for it, and a `CREATE TABLE IF NOT
EXISTS` that finds the table already there leaves the counter it has exactly
where it stood.

A start past what the column can hold is refused. Measured, MySQL creates the
table for `AUTO_INCREMENT=99999999999` on an `INT`, prints the number back, and
answers 1467 for the first row; refusing the statement says the same thing sooner
rather than storing a mark no row could ever be given. So is a value that is not
a plain whole number: measured, `AUTO_INCREMENT=-5` and `AUTO_INCREMENT='7'` are
each 1064, and `AUTO_INCREMENT=1.5` is taken and rounded down, which is a
rounding rule this does not repeat.

A table may write its key as a clause of its own — `PRIMARY KEY (id)` after the
columns — as well as on the column that carries it. That is the spelling MySQL's
own `SHOW CREATE TABLE` prints, so it is the one every dumped schema and every
migration built from a dump writes, while this reads a key only where the column
declares it. The words are moved onto the column named, so the one reader answers
both, and what this prints can be handed back to it.

A key over several columns — `PRIMARY KEY (a, b)`, the join table every schema
with a many-to-many relation has — is read too, and by a different path: one
column's key is moved onto that column, where the table gets a marker of its
own, while several columns' key is written through as the engine's own
`PRIMARY KEY (a, b)`. Measured on 8.4.11 and matched: it prints back as
`PRIMARY KEY (`a`,`b`)` with no space after the comma, both columns report
`PRI`, a row repeating the pair collides and one changing either half does not,
an index written beside it is kept, and an `ALTER` runs against one. A key
column without a nullability clause is stored and printed `NOT NULL`, as MySQL
does; explicit `NULL` and `DEFAULT NULL` remain refused. A counted column
inside one is refused as well, one rowid having no way to spread over a pair.

Measured on 8.4.11 and matched: the printed schema is the same whichever way the
key was written, a key over a column written nullable makes that column `NOT
NULL`, a `CONSTRAINT` name on the key is dropped — the key always being named
`PRIMARY` — an `ASC` on the column is dropped, and the column named is matched
without regard to case. A `USING BTREE` and a `DESC` are printed back, so both
stay refused. A key naming a column the table does not have and a table writing
two keys are refused where MySQL answers 1072 and 1068.

A counted table takes a `FOREIGN KEY` as well, which is the one table-level
constraint an ordinary table takes: a table whose id counts is exactly the table
a child row points at, so refusing one there refused the second statement of
nearly every dumped schema. Measured on 8.4.11 and matched: a counted parent and
a counted child both print back with their keys, the counter runs in both, and a
child row pointing at an id that is not there is refused with the parent's rows
left alone. The other table-level constraints stay refused there.

A table that counts its own ids writes its key as a clause too, and is read the
same way — the words are moved onto the counted column before any of the checks
run. That completes the statement a dump carries for the table nearly every test
suite has: an `int NOT NULL AUTO_INCREMENT` column, an ordinary column, the
`PRIMARY KEY (id)` after them and the trailer after that.

Measured on 8.4.11 and matched: the printed schema is the same whichever way the
key was written, the counter starts and carries on the same way, the counter
shows up in what the table prints, and an index written beside the key is kept.
A key over a column that is not the counted one, and a counted column with no
key at all, are answered 1075 there and refused here. A counted column inside a
key over several columns is taken there and refused here, one rowid having no
way to stand for several columns. The counted column still has to say `NOT
NULL`, which every schema a migration tool writes does.

`ALTER TABLE` runs against a table with an ordinary `PRIMARY KEY`, which is
what nearly every table a test suite migrates has. The durable DDL such a table
is remembered by carries the key inline, and writing that table back was
refused because the renderer that writes a table's MySQL DDL could not write a
`PRIMARY KEY`. It writes the plain inline one now — the shape the checked slice
stores, `INT NOT NULL ... PRIMARY KEY` — so `ADD COLUMN`, `DROP COLUMN`,
`RENAME COLUMN`, `RENAME TO` and a `MODIFY` of any other column all run, and
the key survives them. A `MODIFY` or `CHANGE` of the key column itself is
refused: MySQL keeps the key through one and replacing the column would drop
it.

`MODIFY COLUMN` and `CHANGE COLUMN` restate one column, which is how a migration
widens a type or renames a field. Both read the definition as the whole of what
the column becomes: measured on 8.4.11, `MODIFY COLUMN n BIGINT` over an `INT
NOT NULL DEFAULT 5` leaves a `bigint DEFAULT NULL`, so an attribute the
statement does not restate is gone. `CHANGE` does the same and renames the
column as well. The engine's `ALTER COLUMN old TO <definition>` takes the column
the old one is to become — its name included, which is what carries the rename —
so each becomes one of those, and the rows that were already there are kept.

Repositioning is refused. `FIRST` and `AFTER x` move a column, and where a
column sits is part of what a client reads back, so a statement asking for one
answers rather than quietly leaving the column where it was. A column the table
does not have answers 1054, measured.

`CHECK TABLE` verifies that the stored data reads back, through the engine's own
integrity check, and reports what it found. Measured on 8.4.11: one row of
`<database>.<table>`, `check`, `status`, `OK`, over the same four columns
`ANALYZE TABLE` answers. What the check found instead goes in `Msg_text` under
`error`, which is where MySQL puts it. As with `ANALYZE`, MySQL's statement
names one table and the engine's check covers the whole database file, and the
table is looked up first so a name that is not there answers.

`OPTIMIZE TABLE` is refused. MySQL's InnoDB answers it with a recreate and an
analyze; the engine's nearest thing is a database-wide `VACUUM`, which is far
more than one table was asked for, so the shape needs deciding before it is
written.

`ANALYZE TABLE` refreshes the planner's statistics and reports what it did.
Measured on 8.4.11: one row of `<database>.<table>`, `analyze`, `status`, `OK`,
over four latin1 columns of length 128, 10, 10 and 393216, which this matches.
MySQL's statement names one table and the engine's `ANALYZE` covers the schema,
which is a superset of what was asked for; the table is looked up first, so a
name that is not there answers rather than analysing everything quietly. One
unqualified table at a time is taken — a list, a qualified name,
`NO_WRITE_TO_BINLOG`, `LOCAL` and the histogram clauses are refused.

`SHOW TABLE STATUS` describes each table in the selected database. The eighteen
column shapes are measured on 8.4.11; the values are answered about this server.
`Name`, `Engine`, `Rows`, `Collation`, `Create_options` and `Comment` it can
answer, and every storage figure InnoDB keeps and this does not — `Version`,
`Row_format`, the four lengths, `Data_free`, the three times and `Checksum` —
answers NULL rather than a number invented to look like one. NULL is a shape
MySQL produces here too, for a view. The row count is counted rather than
estimated: MySQL's is an InnoDB estimate, and a real count is the more useful
answer and the only one this can give. A `LIKE` pattern names the tables to
report and a `FROM` or `IN` qualifier names the database, which has to be the
selected one — measured on 8.4.11, the qualifier is spelled either way round and
means the same thing, and a pattern nothing matches answers no rows rather than
an error. The `WHERE` filter is a predicate over the eighteen columns rather
than a pattern, and it is not read.

`SHOW ENGINES` answers with one row. MySQL 8.4.11 lists eleven, most of them
unavailable on the server that lists them; naming MyISAM or CSV here would claim
engines that do not exist, so the row describes the one that does, under the
name `SHOW CREATE TABLE` already reports. The column shapes are measured — six
VAR_STRING columns of length 64, 8, 80, 3, 3 and 3, latin1 collation, the first
three NOT NULL — but the last three values are answered about this server rather
than copied. MySQL's InnoDB row says YES to `Transactions`, `XA` and
`Savepoints`; transactions work here and the other two do not, and a client that
reads those columns before reaching for either is better served by the truth.

`CREATE TEMPORARY TABLE` is taken for an ordinary table, and behaves the way
MySQL's does: measured on 8.4.11, a temporary table shadows a permanent one of
the same name for the connection that made it, and `SHOW TABLES` lists only the
permanent one. The `AUTO_INCREMENT` form is refused, because the allocator is
keyed on a durable table.

`START TRANSACTION READ ONLY` is taken, and the promise in its name is kept.
Measured on 8.4.11: a read inside one works and a write answers 1792, which is
what this answers too — accepting the words and letting the write through would
be the kind of quiet lie a lock that is not held would be. The promise ends with the
transaction. A DDL statement is not held to it, because it commits what came
before and so leaves the read-only transaction before it runs; measured,
`START TRANSACTION READ ONLY; CREATE TABLE u (...)` is taken there too.
`READ WRITE` is the default spelled out and changes nothing.

A standard `mysqldump --databases` restores through the `mysql` client
statement by statement, and every statement MySQL 8.4.11's writes for a schema
of counted tables, a foreign key, `JSON`, `DECIMAL` defaults, `utf8mb4_unicode_ci`
and `utf8mb4_0900_ai_ci` tables, a trigger writing a `CONCAT` of its row, a
view of one table and a view joining two is taken; the rows read back as they
were dumped, and the trigger and the views print as the dump wrote them. So do
an `ENUM`, `DATETIME(6)` values from year 1000 to 9999, and text holding every
escape `mysqldump` writes — `\'`, `\"`, `\\`, `\n`, `\r`, `\0` and `\Z` — a raw
tab, emoji, and the empty word beside NULL. The framework apps' own schema
dumped without `--databases`, with the default options and with
`--single-transaction --routines --triggers --events --hex-blob
--complete-insert --net-buffer-length=4096` (which names every column and
splits a table's rows over several `INSERT`s), and with `--no-data`, replays
into another database the client names, one dump over another, leaving
exactly the rows MySQL 8.4.11 leaves there, as `mysql --batch` prints them.
Besides the settings above, that takes three forms.

The `mysql` client 8.4 keeps comments by default, and read from MySQL's
general log while it restored a dump, it sends each comment line between
statements as a statement of its own — `-- MySQL dump 10.13 ...`, `--`, `--
Dumping data for table ...` — and each statement with the delimiter taken
off. Measured on 8.4.11 by sending each text as one `COM_QUERY`: text holding
only comments — `-- `, `#`, `/* */`, `/*+ */`, a versioned comment naming a
later version than 8.4.11 — optionally followed by semicolons, answers an OK
with nothing affected; it clears the warnings and `ROW_COUNT()` reads 0 after
it, and inside a transaction the transaction stays open. This answers the
same, whatever character set the client named, a comment not being read.
Text holding no comment — empty, whitespace, `;` — is 1065 `Query was empty`,
SQLSTATE 42000, and so is an empty `COM_QUERY`, which was 1064 here. `--`
starts a comment only before a space, a control character or the end;
MySQL's whitespace takes in the vertical tab. A comment after a semicolon —
`; -- a` — is 1064 in MySQL and is left to the statement path here, as is a
versioned comment 8.4.11 runs: `/*!40101 */` is an OK with a deprecation
warning there.

The client also sends `select $$` as it connects, and only when the server
answers 1064 does it read `$tag$ ... $tag$` as a quote while it splits a
script, keeping a `;` between the two. MySQL 8.4.11 answers 1064 for a word
that begins with `$` and holds a second `$` — `$$`, `$a$`, `$$a`, `$é$` —
outside a string, a quoted name and a comment, and so does this; `$`, `$a`,
`a$$`, `x$$y`, `` `$$` ``, `'$$'`, `@$$` and `x.$$` are names, words and
variables there, which this leaves to the statement path. It used to answer
1054 or 1046, which told the client the opposite. MySQL warns 1681 for a name
beginning with `$`, which this does not.

`CREATE DATABASE` takes `IF NOT EXISTS`, which over a database already there
answers OK with note 1007, and the database options described under **A
database's collation** below, the collation among them. A versioned comment is
read as the text it holds, the way `mysqldump` writes the statement:
``CREATE DATABASE /*!32312 IF NOT EXISTS*/ `probe` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci */ /*!80016 DEFAULT ENCRYPTION='N' */``.

**A database's collation.** A database keeps a collation of its own, which
every table made in it takes when the table names neither a character set nor
a collation — what Prisma and Laravel create their database with: `CREATE
DATABASE app CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci`, and Laravel's
``create database `app` default character set `utf8mb4` default collate
`utf8mb4_unicode_ci` ``. `utf8mb4_0900_ai_ci`, the default, and
`utf8mb4_unicode_ci` are the two a database may have, being the two a table
here may have. Measured on 8.4.11:

- The options may each start with `DEFAULT`, take an `=`, come in any order
  and more than once, and name their value bare, in backticks or as a string,
  in any case; `SCHEMA` stands for `DATABASE` throughout. A later `COLLATE`
  replaces an earlier one, and a character set named beside a collation of its
  own leaves that collation, whichever comes first. A character set named
  alone gives the database its default collation, `utf8mb4_0900_ai_ci`.
- A collation MySQL does not have is 1273, a character set it does not have
  1115, a collation beside another character set 1253, and two different
  character sets 1302, each before the database is looked for. `utf8` is read
  as `utf8mb3`, and `utf8_bin` as `utf8mb3_bin`, as MySQL reads them. Every
  other character set and collation MySQL has — `utf8mb4_bin` and
  `utf8mb4_general_ci` among them — and any `ENCRYPTION` but `'N'` are
  refused, a table here being unable to take one.
- A table made in the database naming neither a character set nor a collation
  is exactly the table written `COLLATE=<the database's>`: `SHOW CREATE
  TABLE`, `information_schema.TABLES.TABLE_COLLATION` and
  `COLUMNS.COLLATION_NAME` all say so, and its words compare under that
  collation. One naming only `CHARSET=utf8mb4` takes that character set's own
  default, `utf8mb4_0900_ai_ci`, and `CREATE TABLE ... LIKE` its source's
  collation. `CREATE TABLE ... SELECT` in a database whose collation is not
  the default is refused: MySQL gives the new table the database's collation
  and each column its source column's, spelled out.
- `ALTER DATABASE [name] <options>` and `ALTER SCHEMA` change the collation
  the tables made afterwards take, and nothing already there; a character set
  named alone takes the database back to `utf8mb4_0900_ai_ci`. Without a name
  it changes the selected database, 1046 when there is none. A database that
  is not there is 3503, a statement naming no option 1064, and `READ ONLY`,
  which makes MySQL refuse every write to the database, is refused. It needs
  the grant that lets a session change the database's tables.
- `@@collation_database` reads the selected database's collation as it was
  when the database was selected, and `utf8mb4_0900_ai_ci`, the server's, with
  none selected; the session that alters the database it is in reads the new
  one straight away. An `ALTER DATABASE` another session runs reaches this
  session's next `CREATE TABLE` at once, but this reading only when it selects
  the database again, which is what MySQL does. `@@character_set_database` is
  `utf8mb4`.
- `information_schema.SCHEMATA` answers each database's collation in
  `DEFAULT_COLLATION_NAME`, and `SHOW CREATE DATABASE [IF NOT EXISTS] name`
  prints the statement MySQL prints, naming the database as the statement
  wrote it — measured under `lower_case_table_names=1`, the rule this server
  keeps, `SHOW CREATE DATABASE MIXEDDB` prints `` `MIXEDDB` `` — after the
  same grant `USE` needs; 1049 for a database that is not there.
- A trigger reports, as its `Database Collation` in `SHOW TRIGGERS` and `SHOW
  CREATE TRIGGER`, the collation its database had when the trigger was made,
  and keeps reporting it after an `ALTER DATABASE`. It is kept with the
  trigger's other creation settings, and left out for `utf8mb4_0900_ai_ci`,
  so a trigger stored before is read back unchanged.
- The collation is kept in the root's registry beside the database, which is
  written whole to a new file, synced and renamed over the old one, so it is
  there after a restart and never half written. A database made before
  databases had a collation reads as `utf8mb4_0900_ai_ci`, which is what every
  table made in it was given.

`ALTER TABLE t DISABLE KEYS` and `ENABLE KEYS`, which a dump writes around
every table's rows, answer OK with note 1031, `Table storage engine for 't'
doesn't have this option`, as InnoDB's do, measured; a name that is not there
answers 1146, and a view, which MySQL answers 1347, is refused.

The placeholder view a dump writes first — `SELECT 1 AS name, ...` — is
replaced by the real one at the end, and the real one arrives as three
versioned comments across three lines, which is read as one `CREATE VIEW`
naming its `DEFINER`. A view or trigger whose `DEFINER` is another account than
the one restoring is refused, as MySQL refuses it to an account without
`SET_ANY_DEFINER`.

`START TRANSACTION WITH CONSISTENT SNAPSHOT` is taken, which is what
`mysqldump --single-transaction` opens with — inside the versioned comment
`/*!40100 WITH CONSISTENT SNAPSHOT */`, which the tokenizer expands, so both
spellings arrive the same. The read view is taken at the statement, as MySQL
takes it, so a row committed before the first read stays unseen. Measured on
8.4.11: under `READ COMMITTED` the phrase is ignored with warning 138, and the
transaction begins all the same; so it is here.

`mysqldump --single-transaction` sends that statement before it selects the
database it dumps, which MySQL takes: measured on 8.4.11, `START TRANSACTION`
with no database selected opens a transaction, `UNLOCK TABLES` with nothing
locked and `COM_INIT_DB` leave it open, and `COMMIT` or `ROLLBACK` ends it. A
transaction here belongs to one database's connection, so one begun with
nothing selected is held — the session reports itself in a transaction — and
begun on the database `COM_INIT_DB` or `USE` selects next; `COMMIT` and
`ROLLBACK` end it before then. The one difference is when its read view is
taken: at that selection rather than at the statement, so a row another session
commits between the two is seen, where MySQL would not see it. Selecting the
database already selected inside a transaction leaves it open, as MySQL does
and as a dump does between tables; selecting another is refused, since the
transaction cannot follow the session there — which also refuses a
`--single-transaction` dump of several databases.

`DROP DATABASE` drops a database other sessions still have selected, the way
Prisma's `migrate reset` drops the database its own pooled connections are in
and its schema engine drops the shadow database it is connected to. Measured on
8.4.11 and matched: the drop commits the dropping session's transaction first,
whichever database it names; it waits for a session running a statement on the
database or holding a transaction open that has read it, and a statement another session
starts on the database while it waits waits behind it; a session that merely
has the database selected holds nothing up. Afterwards that session keeps the
name — `DATABASE()` answers it — and a statement on one of the database's
tables answers 1049 `Unknown database 'name'`, until a database is made under
the name again, which it then reads. The session that drops its own database is left in none,
`DATABASE()` answering NULL. A database that is not there answers 1008 `Can't
drop database 'name'; database doesn't exist`, and `DROP DATABASE IF EXISTS`
(or `DROP SCHEMA`) of it answers OK. Measured on 8.4.11 and matched, oddly: that
OK counts one warning, yet `SHOW WARNINGS` lists none and `@@warning_count`
reads 0 after it — where `DROP TABLE IF EXISTS` lists its note 1051 — and under
`sql_notes = 0` it counts none. It commits the transaction first too, and a
session whose database another session dropped is left in none by it, where
the plain form's 1008 leaves it the name. The files are removed while the
other sessions still hold them open, which Unix allows without either side
seeing the other, and nothing runs on them again: every statement first checks
that its database is still there, under the same lock the drop takes. The drop
waits the session's `lock_wait_timeout`, and a drop that runs out answers 1205;
a statement another session starts on the database while the drop waits, and
selecting the database, wait that session's own `lock_wait_timeout` and answer
1205 once it runs out, selecting leaving the session where it was — measured on
8.4.11 and matched. A transaction has read the database once it read one of
its tables or listed them with `SHOW TABLES`, and keeps it until it ends. One
that has only begun, taken a savepoint or run `SELECT 1` lets the drop go at
once, and its next read of a table answers 1049; so does one that read and was
then ended by a `BEGIN`, while a read before a `SAVEPOINT` still holds the drop
after `ROLLBACK TO` it — each measured on 8.4.11 with two sessions and matched.
The engine takes a transaction's snapshot at its first read, so a statement
that leaves one behind has read; a transaction command's snapshot, which `WITH
CONSISTENT SNAPSHOT` takes, is not counted. Two things differ, each listed in
TODO.md: after `WITH CONSISTENT SNAPSHOT`, and after a `SAVEPOINT` taken before
the first read and rolled back to after it, the transaction holds the drop even
though MySQL's has read no table or has let it go.

A session whose database another session dropped runs what reads none of the
database's tables, measured on 8.4.11 with two sessions and matched: `SELECT
1`, `SELECT DATABASE()`, `BEGIN`, `COMMIT`, `SAVEPOINT`, `SET autocommit`,
`UNLOCK TABLES`, `SHOW DATABASES` and `information_schema` reads, which find
no table of the database. What reads or writes one of its tables, makes one, or
lists them — `SELECT ... FROM t`, `INSERT`, `CREATE TABLE`, `CREATE VIEW`,
`CREATE INDEX`, `ALTER TABLE`, `RENAME TABLE`, `SHOW TABLES`, `SHOW CREATE
TABLE`, `DESCRIBE`, `LOCK TABLES` — answers 1049 naming the database, and
`DROP TABLE` and `DROP VIEW` answer what they answer for a table that is not
there, `IF EXISTS` noting it. A transaction open when the database went stays
open, and `autocommit = 0` stays set, also once a database is made again under
the name. Such statements run on a stand-in: an empty database of the same name
held in memory, which refuses every write, so nothing runs on the dropped
database's files. Nor is anything written to them: a session letting its
connection to a dropped database go skips the engine's closing checkpoint, and
the thread that empties large WALs leaves a dropped database alone, both of
which used to copy the WAL into the unlinked database file. A statement
prepared before the drop runs `SELECT 1` meanwhile and answers 1049 for a table,
and once a database is made again under the name it is prepared again over that
one, keeping its number and parameter types — it answers 1146 `Table
'name.t' doesn't exist` until the table is made there, then its rows; measured
on 8.4.11 and matched. A `PREPARE` of `SELECT 1` works meanwhile, and one
naming a table answers 1049.

`SET [SESSION] lock_wait_timeout = <n>` is taken, from one second to a year,
and `DEFAULT`; `@@lock_wait_timeout` and `SHOW VARIABLES` read it back, an
unsigned `LONGLONG` of 21 starting at 31536000, measured on 8.4.11. It bounds
what MySQL calls a metadata lock wait. Measured on 8.4.11 with
`lock_wait_timeout = 1` and `innodb_lock_wait_timeout = 30`, beside another
session's open transaction that wrote a table: `ALTER TABLE`, `DROP TABLE`,
`TRUNCATE TABLE`, `CREATE INDEX`, `RENAME TABLE` and `LOCK TABLES` of that table
each answer 1205 after one second. Here each of those waits for the engine's
one write lock, so it waits `lock_wait_timeout` for it rather than the
`innodb_lock_wait_timeout` an `INSERT` or an `UPDATE` waits. Every 1205 now
carries MySQL's message, `Lock wait timeout exceeded; try restarting
transaction`, where it used to read `database is busy`.

The rest of what `mysqldump` 8.4 sends was read from the oracle's general log
under `--single-transaction --routines --triggers --events --hex-blob
--databases`, and each statement is answered. `SHOW EVENTS` — with the selected
database, 1046 without — `SHOW FUNCTION STATUS` and `SHOW PROCEDURE STATUS`
list no row, there being no stored programs here, in the columns MySQL answers
them in, original tables included; a filter is taken as `LIKE 'pattern'` or
the `WHERE Db = 'name'` a dump writes, and any other `WHERE` is refused, being
a predicate MySQL checks against the listing's columns. MySQL answers 1044 to
`SHOW EVENTS` from an account without the `EVENT` privilege on the database,
measured; there is no such privilege here, and the account's permission to
query the database is what is asked. The query a dump sends
for every table to learn whether it has histograms,
`SELECT COLUMN_NAME, JSON_EXTRACT(HISTOGRAM, ...) FROM
information_schema.COLUMN_STATISTICS WHERE SCHEMA_NAME = ... AND TABLE_NAME =
...`, answers no row — MySQL keeps a histogram only after `ANALYZE TABLE ...
UPDATE HISTOGRAM`, which is refused here — in MySQL's columns, and
`COLUMN_STATISTICS` read any other way is refused. The two
`INFORMATION_SCHEMA.FILES` queries for NDB tablespaces, sent only for an
account with `PROCESS`, stay refused, and a dump carries on past them, as it
does against MySQL for an account without that privilege. `LOCK TABLES
mysql.proc READ`, sent under `--routines` without `--single-transaction`, names
a table MySQL 8 no longer has and answers 1146 there, which a dump ignores;
here it is taken, `LOCK TABLES` locking the selected database whatever it
names. `SHOW FIELDS` of a view, which a dump reads to write the view's
placeholder, is not answered here yet.

`COMMIT AND CHAIN` and `ROLLBACK AND CHAIN` are taken. Each ends the
transaction and begins another at once. Measured on 8.4.11 over an empty table:
`START TRANSACTION; INSERT 5; ROLLBACK AND CHAIN; INSERT 6; ROLLBACK` leaves the
table empty, because the second write was inside the chained transaction; and
`COMMIT AND CHAIN` with autocommit on leaves the session in a transaction even
though there was none to end, so the ending half is skipped rather than the
whole statement. The `AND RELEASE` forms stay refused: MySQL closes the
connection after them, which is a protocol behaviour rather than a statement.

The three savepoint statements are taken: `SAVEPOINT name`,
`ROLLBACK TO [SAVEPOINT] name` and `RELEASE SAVEPOINT name`. Rolling back to a
savepoint undoes only the work since it and leaves the transaction open, which
is the whole difference from a plain `ROLLBACK`, and the status flags say so.
Measured on 8.4.11 and matched by the engine: a savepoint named twice is the
later one, a `ROLLBACK TO` forgets every savepoint taken after the one it names,
and a `COMMIT` forgets them all. A name that is not there answers 1305, SQLSTATE
42000, as MySQL does; MySQL's message names the savepoint where this one does
not, the way every other message here stays fixed. Names are matched whatever
their case on both sides.

With autocommit on and no transaction open, `SAVEPOINT s1` answers OK and
nothing survives it — the statement is its own transaction, so the next
statement's `ROLLBACK TO s1` answers 1305. The engine would instead open a
transaction and hold it across statements, so nothing is run there. With
autocommit off the savepoint is taken inside the session's transaction, opened
first if the session has not written yet, and it does roll back a later write.

Two things about them differ. A savepoint needs the pager's sub-journal, so over
an **in-memory** database the engine takes `SAVEPOINT` and `ROLLBACK TO` and
undoes nothing; the server runs over files, where they work. And a name that is
a reserved word is taken unquoted here — `SAVEPOINT select` — where MySQL
answers 1064 for it.

Two isolation levels are kept, `REPEATABLE READ`, MySQL's default, and
`READ COMMITTED`, which Django asks for on every connection. Both come out of
what the engine already does. An explicit transaction reads from one snapshot
of the whole database, and writes under one write lock over the whole
database. `REPEATABLE READ` holds the snapshot until the transaction ends.
`READ COMMITTED` lets it go before each statement until the transaction writes,
so every statement reads what is committed as it starts. After the first write
the transaction holds the write lock, nothing else can commit, and the
snapshot it has is already the latest.

They are set the ways MySQL sets them. `SET SESSION TRANSACTION ISOLATION
LEVEL`, `SET LOCAL ...` and `SET [SESSION] transaction_isolation = '...'` set the
session's level. `SET TRANSACTION ISOLATION LEVEL` and `SET
@@transaction_isolation = '...'`, with no scope word, set the next transaction
alone. Measured on 8.4.11, and matched here: `@@transaction_isolation` reads the
session's level even inside a transaction running at another; the next-
transaction form answers 1568 inside a transaction, while the session form is
taken there and holds from the next transaction on; and the next-transaction
level is used up by the next transaction that begins — `START TRANSACTION`, or a
statement reading a table, which is a transaction of its own — but not by
`SELECT 1` or by a statement that fails. `READ UNCOMMITTED` and `SERIALIZABLE`
are refused rather than accepted and ignored: a client told yes to either would
go on reasoning about a guarantee it does not have. The `GLOBAL` scope is refused
for the same reason — it changes what other sessions get.

One thing about `REPEATABLE READ` differs, and it is where a snapshot of the
whole database runs out. MySQL reads the latest committed row for an `UPDATE`,
a `DELETE` or a locking read, and goes on reading every other row from the old
snapshot. Measured on 8.4.11: a transaction that read before another session
committed a change to row 2 can still update row 1, and afterwards still reads
row 2 as it was. A snapshot here cannot mix rows from two moments, so that
write fails instead. The transaction is rolled back and answered with 1213,
SQLSTATE 40001 — what MySQL answers for a transaction it has to give up on,
rolling it back the same way — and a client that retries a transaction on 1213
or 40001 recovers by running it again. Prisma reports it as P2034 and leaves
the retry to the application. It happens only when another session committed
between the transaction's first read, by an earlier statement, and its first
write.

What is not a read here does not start the snapshot either. Preparing a
statement reads this server's own catalog and not the tables, and MySQL takes
no read view for it: measured on 8.4.11, a session that begins, prepares an
`INSERT` and a `SELECT`, and runs them after another session committed, sees
that session's row and writes its own. The same holds here — Prisma's
`Promise.all` of two `create`s runs that way over two pooled connections, and
was answered 1213 while a prepare took the snapshot. A statement that begins
with no snapshot — any with autocommit on, the first of a transaction, every
one under `READ COMMITTED` — takes its own, often by reading the catalog or
taking a savepoint before it writes, and may then wait for another session's
write. When that session commits the snapshot is stale, and the statement is
run again from the start on a new one: nothing it read reached the client and
a stale snapshot has never written, so that is the same as running it after
the other session committed. Before, even an insert with autocommit on into a
table that counts its ids answered 1213 that way. Measured on
8.4.11, two transactions inserting different rows both commit, and an insert
of a key another open transaction just wrote answers 1062 once that one
commits, leaving the transaction open; both hold here, except that the second
insert waits for the first transaction's commit where MySQL's does not wait at
all, the write lock being one over the whole database. So under `READ
COMMITTED`, whose snapshot is new at every statement, a write is no longer
given up at all. Under `REPEATABLE READ` one thing still counts as a first read
where MySQL's does not: a `SAVEPOINT`. Measured on 8.4.11, a transaction that
begins with one still sees a row another session commits after it; here the
savepoint takes the snapshot, so such a transaction reads and writes as though
it had read at the savepoint.

A `DATE` holds the day alone. Measured on 8.4.11: the column reports type 10
with length 10, the width of `YYYY-MM-DD`, decimals 0, the binary collation and
the binary flag, and `SHOW CREATE TABLE` prints `date`. `CURDATE()` and
`CURRENT_DATE` each answer the same type and width with the NOT NULL flag
added. Both spellings of the older reading work too — `CURRENT_TIMESTAMP`
without its parentheses had the same missing argument list standing in its way
and now answers what `NOW()` answers.

A `DATE` takes what MySQL takes and stores what MySQL stores: measured,
`'2026-9-6'`, `'20260906'`, `'26/9/6'` and `'2026-09-06 01:02:03'` each store
`2026-09-06`, and a time written after the day is read and checked before it is
dropped, so `'2026-09-06 25:00:00'` is refused for the hour it names. The
calendar is checked the same way: `'2026-02-30'` answers 1292, naming the value
an incorrect date, which is what MySQL calls it. A `WHERE` comparison against a
canonical `DATE` value is supported, including a bound date parameter.

A `TIME` holds a span rather than a moment, and the difference shows in what it
takes: measured on 8.4.11 it runs from `-838:59:59` to `838:59:59`, so it takes
a sign and more than a day. `TIME(fsp)` accepts precision 0 through 6 and keeps
the declared fractional digits. The column reports type 11 with length 10 plus
the fractional point and digits when fsp is nonzero, decimals equal to fsp and
the binary flag; `SHOW CREATE TABLE` prints `time` or `time(fsp)`.
`CURTIME()` and `CURRENT_TIME` answer the
same type at length 8, the width of a clock reading, with NOT NULL added — the
same type is narrower there because a reading holds no span past a day. Over the
binary protocol a `TIME` is its own field form, carrying a sign byte and the
whole days its hours run past, so `838:59:59` crosses as 34 days and 22 hours.
`'99:99:99'` answers 1292, naming the value an incorrect time. The looser
spellings are taken and stored the way MySQL stores them, and a `TIME` reads
unlike a `DATE`: only a colon separates its fields, so `'12.34.56'` is refused
where the same text after a day is taken; without a colon the digits are read
from the right, so `'5'` is five seconds and `'123456'` is `12:34:56`; and a
number with a space and a digit after it is a count of days, so `'2 1:1:1'` is
`49:01:01`. Equality, inequality, null-safe equality and `IN` comparisons
against a canonical `TIME` value are supported; ordering comparisons remain
refused because lexicographic order does not order signed spans correctly.

A `YEAR` is the odd one among the temporal types, and the oddity is measured: it
carries the flags of a number rather than of a moment — unsigned, zerofilled and
numeric — and **no** binary flag, which every other temporal column has. It
reports type 13 at length 4, the four digits it prints, and crosses the binary
protocol as the two bytes a SHORT does. It runs from 1901 to 2155, and 1900 or
2156 answers 1264. A year under a hundred names one in the window MySQL keeps,
so 69 is 2069 and 70 is 1970, and the zero is where a year written as text
parts from one written as a number: measured, `'0'` is 2000 and 0 is the zero
year, which reads back as `0000`.

`YEAR(column)`, `MONTH(column)` and `DAY(column)` read a part out of a date.
Measured on 8.4.11: `YEAR` answers a `YEAR` of length 4 carrying the unsigned,
binary and numeric flags — and **no** zerofill, which the column does carry —
while `MONTH` and `DAY` each answer a `LONGLONG` of length 3. All three report
no NOT NULL flag even over a NOT NULL column, which is what MySQL reports. Each
is answered only over a column holding a date: measured, `YEAR` over a `TIME`
answers the current year, which is a coercion rather than a reading, so that
and every other argument is refused.

`DATE_FORMAT(column, 'format')` writes a moment out. The engine's own
`strftime` answers a few of MySQL's specifiers and none of the rest — no month
or weekday name, no twelve-hour clock, no week number — so the whole of it is
written by the frontend instead, from the moment the column holds. Every
specifier MySQL writes is written: `%Y %y %m %c %d %e %D %H %k %h %I %l %i %s
%S %p %r %T %j %W %a %M %b %w %f`, the four week numbers `%U %u %V %v` with
their years `%X %x`, `%%`, and an unknown specifier's own letter, which is what
MySQL writes for one. The format has to be a literal, because the answer's
width is worked out from it: measured, a VAR_STRING as wide as the format could
make it, with the text collation and no flags at all, where `%H` alone reserves
seven characters because a time may run past a day. The column has to hold a
day or a moment.

`LOCATE` takes the place to start from that MySQL takes: measured, it counts
from the front of the whole haystack however far in it was told to start, and a
start before the first character finds nothing. The place has to be a literal.

`HEX` writes a number in hexadecimal and text as its bytes, and which it is is
asked at the row rather than worked out from the column, so a column holding
either answers correctly. Measured: a fractional number is rounded first, so
`HEX(1234.56)` is `4D3`, and a negative is written as the sixty-four bits it
holds, so `HEX(-1)` is `FFFFFFFFFFFFFFFF`. Over a column holding a number the
result reports 64 whatever the number's width is.

`CONCAT_WS(separator, ...)` is as wide as its parts laid end to end with the
separator in each gap: measured over a `VARCHAR(20)` and a `VARCHAR(30)`,
`CONCAT_WS('-', a, b)` reports 204 and `CONCAT_WS(', ', a, 'lit', b)` 228, and a
number counts its own width, 11 for an `INT`. It skips a NULL part and keeps an
empty one, so two empty words answer `-` and a row whose every part is NULL
answers an empty word rather than NULL; it is nullable all the same. The
engine's `concat_ws` does each of those. The separator has to be a written
word — a NULL one makes every row NULL — and every part a column of words or of
whole numbers, or a written word.

`SUBSTRING(col, from [, count])` and its `SUBSTR` spelling read characters. The
place and the count have to be written numbers, because the width is worked out
from them. Measured over a `VARCHAR(100)`: what is left of the column after the
place, held to the count — `SUBSTR(name, 2)` reports 396, `SUBSTR(name, -10)`
40, `SUBSTRING(name, 99, 5)` 8 and `SUBSTR(name, 2, 3)` 12. A place of 0, a place
reaching back past the start, a place past the end and a count below one each
answer an empty word, and report 0 when the column says so: `SUBSTR('apple', 0,
3)`, `SUBSTR('apple', -6)`, `SUBSTR('apple', 6)` and `SUBSTR('apple', 2, 0)` are
all empty. The engine's `substr` reads the first two as the start, so the call
is answered by the dialect instead.

`SUBSTRING_INDEX(col, delimiter, count)` answers the part before the count-th
delimiter, or after it counting from the end. It reports a `VAR_STRING` as wide
as its column (a `CHAR(8)` reports 32), nullable even over a NOT NULL column.
Measured on 8.4.11: the delimiter is matched by its bytes, so case counts —
`SUBSTRING_INDEX('aXbxcXd', 'x', 1)` is `aXb` — and copies of it are found from
the left without overlapping whichever way the count runs, so
`SUBSTRING_INDEX('aaaaa', 'aa', -2)` is `aaa`. A count of 0 or an empty
delimiter answers an empty word, a count past the copies there are answers the
whole word, and a NULL argument answers NULL. The count has to be written; the
delimiter can be written or bound. In a `WHERE` or an `ORDER BY` it compares the
way its column compares, so `SUBSTRING_INDEX(email, '@', -1) = 'X.COM'` finds
`a@x.com`.

`MD5`, `SHA1` (and `SHA`) and `SHA2` answer the digest of a column of words in
lower-case hexadecimal: a `VAR_STRING` of 128, 160, and for `SHA2` 224, 256, 384
or 512 as it names 224, 256 (or 0), 384 or 512 bits. A column of numbers is
refused, because MySQL digests the number written out and the engine would
answer NULL, and so is any other `SHA2` size, which MySQL answers NULL with a
warning for.

Refused beside these: each over a `TEXT`, which MySQL answers as a
`MEDIUM_BLOB` of 1048560; each over a `DECIMAL` or a real column; a place, a
count or a `SHA2` size read from the row; and a call compared against a `?`,
which carries no kind until it binds.

`col % n`, `col DIV n` and `-col` are taken over a column of whole numbers.
Measured on 8.4.11, each answers a `LONGLONG` as wide as the column — 4 over a
`TINYINT`, 6 over a `SMALLINT`, 11 over an `INT`, 20 over a `BIGINT`, and 4
over a `TINYINT(1)`, which reports 1 on its own (`MOD` now reports that 4
too). A negation keeps the column's `NOT NULL`; `%` and `DIV` never do, since
a zero divisor answers NULL. `%` keeps the dividend's sign and `DIV` cuts
toward zero — `-7 % -3` is -1 and `-7 DIV 2` is -3 — which the engine's `%` and
`/` over whole numbers do too. The divisor has to be a written whole number.
Refused: a zero divisor, which MySQL answers NULL for with a warning this does
not raise; `DIV -1`, and `-col` over a `BIGINT`, which MySQL answers 1690 for at
the smallest `BIGINT` and the engine a real number; and each over a `DOUBLE` or
a `DECIMAL`, which MySQL answers as a `DOUBLE` or a `DECIMAL` (`MOD` over a
`DOUBLE` too, which used to report a whole number). A negation names its
column `-i`, as MySQL does. `-col` orders rows too — `ORDER BY -qty`.

`ROUND(col [, places])` takes a written number of places, and none means 0.
Measured on 8.4.11, what it answers is the kind of number its column holds:

- over a whole number a `LONGLONG` of 21, rounding half away from zero left of
  the point (`ROUND(15, -1)` is 20, `ROUND(-25, -1)` -30, `ROUND(2147483647,
  -1)` 2147483650) and leaving the number alone right of it. A `BIGINT`
  rounded left of the point is refused: past the largest one MySQL answers
  1690;
- over a `DOUBLE` a `DOUBLE` of 23, worked out the way MySQL's `my_double_round`
  works it: the number is scaled by the power of ten the places name, rounded
  half to even, and scaled back. So `ROUND(2.5)` is 2, `ROUND(0.15e0, 1)` is 0.2
  because 0.15 times ten is 1.5 exactly, `ROUND(0.25e0, 1)` is 0.2,
  `ROUND(1.005e0, 2)` is 1 because 1.005 times a hundred falls short of 100.5,
  and `ROUND(2.675e0, 2)` is 2.68. The engine's `round` rounds half away from
  zero, so the dialect answers the call. `ROUND(x)` over a `DOUBLE` used to
  answer a whole number rounded half away from zero, a `LONGLONG` where MySQL
  answers a `DOUBLE`;
- over a `DECIMAL` a `DECIMAL` rounded half away from zero, whose scale is the
  places held to the column's own and whose width gains a whole digit for the
  carry when a place is cut away: over a `DECIMAL(10,3)`, `ROUND(d)` reports 9,
  `ROUND(d, 1)` 11 with a scale of 1, and `ROUND(d, 5)` the column's own 12 and
  3. `ROUND(-0.155)` is 0, not -0. A place left of the point is refused.

`FLOOR`, `CEIL` and `CEILING` answer a `LONGLONG` of 21 over a whole number and
a `DOUBLE` of 23 over a `DOUBLE` — they used to answer a whole number for both.
Over a `DECIMAL` they answer a `LONGLONG` of 21 too while its whole digits, and
one more where it has a fraction, number no more than eighteen — measured on
8.4.11, `DECIMAL(18,0)` and `DECIMAL(10,2)` do and `DECIMAL(19,1)` does not —
NOT NULL over a NOT NULL column and signed over an unsigned one. The fraction
is cut off exactly and the whole number moved one down or up where one was
cut, since the engine's own `floor` would read the `DECIMAL` as a double: over
a `DECIMAL(10,2)` holding -1.50, `FLOOR` is -2 and `CEIL` -1. A wider `DECIMAL`
answers a `NEWDECIMAL` and is refused.

A `DOUBLE` rounded to a negative zero is written `-0`, as MySQL writes it —
measured, `ROUND(-0.4e0)` and `CEIL(-0.5e0)` are both `-0`, and so is a `-0e0`
stored in a `DOUBLE` column. Every `DOUBLE` used to be written without the sign
of a zero; a written `-0.0` is a `DECIMAL`, which stores a zero with no sign in
both engines, so what is stored reads back `0` as it did.

A comparison, `NOT col` and `col IS TRUE` — with `IS FALSE`, `IS NOT TRUE` and
`IS NOT FALSE` — stand as result columns. Measured on 8.4.11, each answers a
`LONGLONG` of length 1, 1, 0 or NULL, named as written; a comparison or a
`NOT` is NOT NULL only where nothing it reads can be null (`id > 1` and `NOT id`
over a key, `COUNT(*) > 0` always), and a truth test always is, NULL being
neither true nor false. The same shape stands beside other columns. A
comparison is a column against a written number or word, written the way a
`WHERE` writes it — so a word is compared under the column's collation and
`name = 'APPLE'` is 1 over `apple` — or a `COUNT` against a written whole
number. So are the shapes a report of how old each row is writes, each read
the way a `WHERE` reads it: a column against a reading of the clock
(`NOW() > created_at`, NOT NULL where the column is), a column against a
shifted reading (`DATE_SUB(NOW(), INTERVAL 1 DAY) < created_at`), a
`DATEDIFF` or `TIMESTAMPDIFF` against a written whole number, a subquery's
`COUNT` against one (`(SELECT COUNT(*) FROM posts) > 0`), and two written
whole numbers (`2 > 1`, NOT NULL). Measured on 8.4.11, the shifted reading,
the counts of days or units and the subquery are nullable whatever they read,
and a NULL moment answers NULL in both engines. A subquery's count beside a
table read with an `ORDER BY` over a bare column is refused, the `ORDER BY`
wanting the table's types, which a statement with a subquery is not read
with. `NOT` and the truth tests read a column of whole numbers or a `DOUBLE`,
each read as true where it is not zero; a word is refused, MySQL reading it as
the number it begins with (`NOT 'apple'` is 1), and so is a `DECIMAL`. In a
`WHERE`, `flag IS TRUE` and its kin test whatever the `WHERE` reader takes, so
`WHERE (qty > 0) IS FALSE` keeps a row whose `qty` is not above zero and drops
one where it is NULL.

`RAND()` answers a double between zero and one, NOT NULL, as MySQL's does. A
seeded `RAND(n)` is refused: the engine has no seeded random, so answering one
would answer a different sequence. `UUID()` answers a thirty-six character
identifier — the engine's is a random one where MySQL's is time-based, so the
two differ in kind while both are identifiers.

`STR_TO_DATE(column, 'format')` reads that back, and what it answers is the
format's doing rather than the text's: measured, a format naming only day parts
answers a `DATE` of length 10, one naming only clock parts a `TIME` of 10, and
one naming both a `DATETIME` of 19, each with the binary collation and flag. The
format has to be a literal, because it is what says which. Text that runs out
before the format does leaves the rest of it reading nothing, so `'2026-09-06'`
by `'%Y-%m-%d %H:%i:%s'` is midnight on that day; text left over after the
format is read is ignored; and text the format cannot read answers no value at
all. A format naming a week number or the weekday number is refused: MySQL takes
those and they name no day on their own, so reading one would mean answering a
day it did not name.

`HOUR`, `MINUTE` and `SECOND` read the clock out of the same moment: measured,
`MINUTE` and `SECOND` answer a `LONGLONG` of length 3 as `MONTH` does, and
`HOUR` one of length 4, its span running past a day. A `TIME` column is left
out of these for a reason of its own rather than the coercion one: it holds a
span running to 838 hours, which MySQL reads out whole and the engine's reader
cannot. `DATEDIFF(b, a)` answers the days between the two as a `LONGLONG` of
length 9, counting the date alone and dropping any time either carries —
measured, `2024-01-15 10:00:00` to `2024-02-15 09:59:59` is 31 days where
`TIMESTAMPDIFF` counts 30 — and every column it names has to hold a date.

`DATE_ADD(column, INTERVAL n unit)` and `DATE_SUB` shift a date. Measured on
8.4.11: an interval of whole days, months or years keeps the column's own kind
— a `DATE` stays a `DATE` of length 10 and a `DATETIME` keeps its time — while
an interval carrying an hour, a minute or a second answers a `DATETIME` of
length 19 either way, or wider by the fraction of a second the column holds.
Which kind the shift answers therefore depends on the column, and the
rendering layer does not know column types, so the stored text says it
instead: a `DATE` is exactly the ten characters of `YYYY-MM-DD`. A `TIME`
column is refused for holding no date to shift.

A user variable is the connection's own. `SET @x = 1` holds a value and
`SELECT @x` reads it back; another connection never sees it, and
`COM_RESET_CONNECTION` takes it away, both measured on 8.4.11. Names are matched
whatever their case, while the result column is named after the variable as the
client wrote it, so `SELECT @X` answers a column called `@X` holding what
`SET @x` put there. A variable never set answers NULL rather than an error.

What the column says depends on what was stored, and MySQL is not consistent
about it. Measured on 8.4.11:

| Held | Type | Length | Decimals | Collation | Flags |
|---|---|---|---|---|---|
| An integer | `LONGLONG` | 21 | 0 | binary (63) | `BINARY` `NUM` |
| A decimal | `NEWDECIMAL` | 67 | 30 | binary (63) | `BINARY` `NUM` |
| A string | `MEDIUM_BLOB` | 16777215 | 31 | **latin1_swedish_ci (8)** | **none** |
| NULL | `MEDIUM_BLOB` | 16777215 | 31 | binary (63) | `BINARY` |
| Never set | `VAR_STRING` | 65535 | 31 | binary (63) | `BINARY` |

The string row is the odd one: it reports latin1_swedish_ci where every other
column this server builds reports utf8mb4, and it carries no flag at all, not
even the blob flag a `MEDIUM_BLOB` would otherwise have. The decimal keeps the
digits as they were written, so `SET @f = 1.50` reads back `1.50`.

Only a literal is taken. `SET @y := @x + 1` and `SET @x = (SELECT ...)` are
refused rather than half-answered, as is a projection that mixes a variable with
anything else — `SELECT @x, id FROM t` — and MySQL's assignment inside a
projection, `SELECT @x := id FROM t`. Both `=` and `:=` spell the assignment,
and one statement can set several variables.

A result column's collation and width follow the **connection's** character
set, not the column's, which is a thing to know before measuring anything
here. MySQL scales both to `character_set_results`: measured on 8.4.11,
`SHOW ENGINES` reports its `Engine` column as latin1_swedish_ci of width 64
to a client connected in latin1 and as utf8mb4 of width 256 to one connected
in utf8mb4, and `SET @s = 'abc'; SELECT @s` answers a `MEDIUM_BLOB` of
16,777,215 to the first and a `LONG_BLOB` of 268,435,440 to the second. The
`mysql` client defaults to latin1 unless told otherwise, so every measurement
of a text column has to pass `--default-character-set=utf8mb4`. This server
speaks utf8mb4 and only utf8mb4 — `SET NAMES` refuses every other name — so
the utf8mb4 numbers are the ones it answers with. The type moves with the
charset too, MySQL choosing a blob's width by its byte count: what a latin1
client is told is a `MEDIUM_BLOB` a utf8mb4 one is told is a `LONG_BLOB`.
This server reports utf8mb4_general_ci where MySQL 8.4's default is
utf8mb4_0900_ai_ci, which is the collation it claims everywhere and is
written up under the known divergences.

An `ENUM` is taken, members and all. The engine's declared type name is what
carries a MySQL type through this frontend — it is how `INT UNSIGNED` and
`VARBINARY(255)` survive — and the engine's own grammar takes a number inside
a type's arguments and nothing else, which is why `ENUM('a','b')` looked
impossible. It takes a **quoted** type name whole, though, and gives it back
unchanged, so the MySQL type is written as one and the members ride the same
carrier as everything else. The values are stored as the text they are.

Measured on 8.4.11: the column reports the fixed-width string type (254) with
the ENUM flag, the connection's collation, and the width of its longest
member counting the four bytes utf8mb4 reserves — `medium` reports 24. `SHOW
CREATE TABLE` and `SHOW COLUMNS` both print `enum('small','medium','large')`,
the keyword in lower case and the members as written. A value that is not a
member answers 1265, SQLSTATE 01000, and the comparison ignores case as the
column's collation does.

An `ENUM` orders by the member's **declared position**, not by the member text,
so `small, medium, large` come back in that order. A NULL sorts in front of
everything and the empty error member in front of the declared members, both
measured. A comparison is a different rule and MySQL's own: measured,
`WHERE size > 'small'` answers nothing, because an `ENUM` compared against a
string compares as a string. A `DEFAULT` on an `ENUM`, one used as a key, and a
member holding a quote or a backslash are each refused rather than
half-answered.

A `SET` orders by its numeric value, one bit for each declared member, as it
does in MySQL. NULL and the empty set sort before nonempty values.

A `SET` rides the same carrier and differs in what it stores: any subset of
its members, joined by commas. Measured on 8.4.11: the column reports the
fixed-width string type with the SET flag and the width of every member laid
end to end with the commas that would join them — `read`, `write` and `exec`
report 60 — and `SHOW CREATE TABLE` prints `set('read','write','exec')`. The
empty string is the empty set and is stored.

Neither an `ENUM` nor a `SET` keeps the text it was written with: MySQL reads
the value and writes back what it read, and so does this. A member is matched
ignoring case and ignoring trailing spaces and stored in the spelling the
column declares, so `'SMALL'` and `'small  '` both store `small`. A `SET`
value's members come out in declared order with each named once, so
`'exec,read'` stores `read,exec` and `'read,read'` stores `read`. A value
naming no member is read as a number instead — for an `ENUM` a declared
position, where `'0'` is the empty error member MySQL keeps in front of them,
and for a `SET` one bit for each member, so `'3'` stores `read,write`. A space
around a comma, an empty member, and a position or a bit past the last member
each answer 1265. Every reading measured on 8.4.11.

MySQL does not always refuse a value wider than its column, and the two cases
where it does not are answered here now. An overflow made only of trailing
spaces is cut back to the declared width and reported as note 1265, so
`'abcd  '` stores `abcd` in a `VARCHAR(4)`; and a `CHAR` gives back no trailing
space at all, whatever it was written with, so `'ab  '` in a `CHAR(4)` reads
back as two characters and four spaces read back as none. An overflow with
anything but spaces past the width still answers 1406. A `VARBINARY` counts
bytes and has no space rule: `'abcd '` in a `VARBINARY(4)` answers 1406.

A `JSON` column holds a document rather than the text it was written with.
MySQL parses a document on the way in and stores what it parsed, so what a
client reads back is never quite what it wrote, and this does the same: an
object's keys come out shortest first and then by their bytes, a key written
twice keeps only the value written last, spacing is one space after every
colon and comma, and a number is printed from the value it parsed to.
Measured on 8.4.11: `{"b":1,"a":2}` reads back as `{"a": 2, "b": 1}` and
`[1,  2,3]` as `[1, 2, 3]`. Text that is not a document answers 3140; MySQL
names what its parser found and where, and the message here stays fixed, as
every other one does. The column reports type 245 with the widest length
there is, the binary collation, and the blob and binary flags.

`CAST(value AS JSON)` written whole into a `JSON` column, in an `INSERT`'s
`VALUES` or an `UPDATE`'s `SET`, is how GORM writes a `datatypes.JSON`, with
the value bound. Measured on 8.4.11 with go-sql-driver's binary types: a bound
word is parsed and stored the way MySQL stores a document, a bound whole
number as that JSON number, and NULL as NULL; a word that is no document, the
empty word among them, fails with 3141 and writes nothing. This does the same
and refuses the word MySQL answers 3141 for, as it refuses a written one; a
bound double, whose JSON spelling was not measured past one value, is refused
too. The cast is taken only where its column is a `JSON` one.

`JSON_EXTRACT(doc, '$.path')` reads one path out of a document and answers the
JSON value it found, so a string comes back with its quotes;
`JSON_UNQUOTE(JSON_EXTRACT(...))` takes those quotes off; and `JSON_VALID`
answers one or zero. Measured on 8.4.11: the first reports the JSON type at
length 4294967292, the second a LONG_BLOB at the widest length there is, both
with the text collation and the binary flag, and the third a LONGLONG of 21
with the binary collation. The paths taken are the plain member-and-element
ones — `$`, `$.a`, `$[0]`, `$.a[1]`; MySQL's wildcards, `$.*`, `$[*]` and
`$**`, are refused rather than read a different way, and so is a call naming
more than one path. MySQL's operator spellings of the first two, `doc -> '$.a'`
and `doc ->> '$.a'`, read the same and are named after the text they were
written with, as MySQL names them.

Each reading is read by the dialect, the way a condition reads it, rather
than by the engine's own `->` and `->>`. Measured on 8.4.11: `[0]` over a
lone value is the value, so `$.s[0]` over `{"s": "x"}` is `"x"` and `$.o[0]`
over an object is the object, where the engine answered no value; the JSON
null unquotes to the word `null` and `true` to `true`, where the engine
answered no value and 1. Laravel's `select('profile->city as city')` reads a
member through `json_unquote(json_extract(...))` and answered NULL for a
member holding the JSON null.

SQLAlchemy reads a member as text through `CASE JSON_EXTRACT(col, 'path') WHEN
'null' THEN NULL ELSE JSON_UNQUOTE(JSON_EXTRACT(col, 'path')) END`, both
readings the same, which is what `profile["city"].as_string()` writes. Measured
on 8.4.11: it reports the column `JSON_UNQUOTE` reports and answers what that
answers, except no value where the path finds the JSON null — the JSON string
`"null"` still answers the word. `as_integer()` writes `CAST(JSON_EXTRACT(...)
AS SIGNED INTEGER)` in the `ELSE` and `as_float()` `JSON_EXTRACT(...)
+0.0000000000000000000000`, in a projection and in a condition alike. Measured
on 8.4.11: the first reports a LONGLONG of 21 and the second a DOUBLE of 23,
both with the binary and numeric flags; a whole number is itself — one past
the signed range wrapping around, `18446744073709551615` reading as -1 and as
`1.8446744073709552e19` — a double is rounded half to even into a whole
number (`20.5` is 20, `2.5` 2, `-0.5` 0), `true` and `false` are 1 and 0, and
a string reads as the number it spells, with spaces before it and a sign
allowed (`" -12"`, `"+5"`, and `"1e3"` and `".5"` as doubles). MySQL reads
everything else with warning 3155 or 3156 and a value of its choosing — a
double past the signed range as a whole number, a string spelling anything
else, spaces after it included, an array and an object — and the statement is
refused when it reads one. Either is compared with a written number, as a
number. `as_numeric()`'s `CAST(... AS DECIMAL(p, s))` and `as_boolean()`'s
`WHEN true THEN true ELSE false` are refused; see TODO.md.

`JSON_TYPE` names a document's kind, `JSON_LENGTH` counts what it holds at the
top, `JSON_KEYS` answers an object's keys as a document of their own, and
`JSON_QUOTE` writes text as a JSON string. Each of the first three reads a
whole column or one path out of it. The engine's own JSON functions answer
each of these differently — `object` where MySQL says `OBJECT`, a length for
arrays alone, and no keys at all — so each is read here rather than passed
through. Measured on 8.4.11: `JSON_TYPE` answers `OBJECT`, `ARRAY`, `STRING`,
`INTEGER`, `UNSIGNED INTEGER`, `DOUBLE`, `BOOLEAN` and `NULL`, where the last
is the JSON null and not the absence of an answer; `JSON_LENGTH` counts only
the top level, so `[[1,2],[3]]` is two and `{"a":{"b":1,"c":2}}` is one; and
`JSON_KEYS` answers no value at all for anything but an object.

`UNIX_TIMESTAMP` counts the seconds from the epoch to a moment and
`FROM_UNIXTIME` reads one back. MySQL reads both in the session's time zone;
this frontend accepts these calls only in UTC sessions. Measured on 8.4.11 with
`time_zone = '+00:00'`: a `DATE` reads as its midnight, a moment before the
epoch counts 0 rather than a negative, where the engine counts backwards, and a
negative count reads no moment at all, where the engine reads one before the
epoch, and so does a count past 32536771199, `3001-01-18 23:59:59`, the last
moment MySQL reads, where the engine reads on. The count is a LONGLONG of 21
with the binary and numeric flags —
NOT NULL for `UNIX_TIMESTAMP()`, which reads now and so has nothing that could
be null — and the moment a DATETIME of 19 with the binary flag alone.

The count is read out of a `DATE`, a `DATETIME` or a `TIMESTAMP` and the moment
out of a whole number; MySQL reads either by coercing the other, which this has
not measured.

`FROM_UNIXTIME(n, format)` writes the moment out the way `DATE_FORMAT` does,
which is how a report shows a moment an application stored as seconds. Its
width looked like a rule of its own and is not: measured on 8.4.11, it reports
exactly the `VAR_STRING` `DATE_FORMAT` reports for the same format — 40 for
`'%Y-%m-%d'`, 516 for `'%W %M'`. The count is a whole-number column or a written
whole number, and the format a written word.

`TRUNCATE` cuts a number off at a count of places, rounding nothing. Measured
on 8.4.11: `TRUNCATE(1.999, 2)` is `1.99`, and a negative count zeroes digits
left of the point, so `TRUNCATE(1234.5678, -2)` and `TRUNCATE(1234, -2)` are
both `1200`. The cut is made on the decimal a reader would have written rather
than on the double behind it, which is what the engine's own arithmetic would
cut: 0.29 times a hundred is 28.999999999999996, and cutting that answers 0.28
where MySQL answers 0.29. The result is a LONGLONG of 21 over an integer column
and a DOUBLE of 23 over a `FLOAT` or a `DOUBLE`, both carrying the binary and
numeric flags. Over a `DECIMAL` it is a `DECIMAL` of its own: the scale is the
count it was asked for held to the column's, and the width is the column's
whole digits plus a sign, plus the fraction and its point where there is one —
measured, `DECIMAL(10,3)` cut at two reports 11 with a scale of 2, at five the
column's own 12 and 3, and at zero or below 8 with no scale at all. A text
column is refused, and so is a count read from a column rather than written
out.

`FORMAT` writes a number for a person to read: rounded, its integer part
grouped in threes, and carrying exactly the digits it was asked for. The engine
has no grouping of any kind, so the whole of it is written by the dialect.
Measured on 8.4.11: `FORMAT(1234.5678, 2)` is `1,234.57`, half goes away from
zero rather than to the even digit — `0.5`, `1.5` and `2.5` are `1`, `2` and
`3` — a value that rounds to nothing loses its sign, `FORMAT(-0.4, 0)` being
`0`, a negative count answers no fraction at all rather than rounding to a
whole ten, and asking for forty digits answers thirty.

A `DECIMAL` is rounded as the exact decimal it is, half away from zero, rather
than through a double: measured over a `DECIMAL(10,3)`, `FORMAT(1234567.125, 2)`
is `1,234,567.13`, `FORMAT(9999999.995, 2)` is `10,000,000.00`,
`FORMAT(-0.005, 2)` is `-0.01`, `FORMAT(-0.004, 2)` is `0.00` with no sign and
`FORMAT(-0.004, 5)` is `-0.00400`. The engine rounds it with its decimal
rounding and the dialect groups what that writes.

The result is a VAR_STRING whose width is the column's own length plus a comma
for every three of its digits plus thirty-two, and the count does not change
it — measured, 184 over an `INT` of 11, 232 over a `BIGINT` of 20, 244 over a
`DOUBLE` of 22, and the same 192 over a `FLOAT` of 12 and a `DECIMAL(10,3)` of
12. The count is written out rather than read from a column, and a text column
is refused: MySQL formats one by coercing it, which this has not measured.

`JSON_CONTAINS_PATH` answers whether the paths it is given are there, one of
them or all of them, and `JSON_CONTAINS` whether a document holds another.
Measured on 8.4.11: a member holding the JSON null counts as being there, so
the first is answered through the engine's `json_type` at a path — which tells
a member holding a null from a member that is not there — rather than through
a reading, which answers nothing for both. The keyword is read without regard
to case, and anything but `one` or `all` is refused where MySQL answers 3154.

Containment is the rule MySQL's own documentation gives, and it is answered by
the dialect because the engine has none of its own: a candidate array is held
by a target array when every one of its elements is held by some element of the
target, a candidate that is not an array is held by a target array when some
element holds it, a candidate object is held by a target object when every one
of its members is there by name with a value that is held, and anything else is
held only by something equal to it. Two numbers are equal when they count the
same — measured, `JSON_CONTAINS('1', '1.0')` is 1. A third argument names the
part of the target to look in, and a path the target does not have answers no
value at all rather than 0, as does a NULL document. Both report a LONGLONG of
21 with the binary and numeric flags.

`JSON_SEARCH` answers the paths to the strings a pattern matches. Measured on
8.4.11: only strings are looked at, so a number is never found; the match tells
one case of a letter from the other, unlike `LIKE` over a text column, so `X`
is not found by `x`; `one` answers the first path as a JSON string and `all` an
array of them, except that a single match answers the one path on its own; and
nothing found answers no value at all. A path is written the way MySQL writes
one, a key bare when it reads as a name and in quotes when it does not, so
`{"my key": "x"}` is found at `$."my key"`. The escape character is MySQL's
fourth argument and a backslash where it is not written. The fifth argument and
beyond name paths to search inside, which are not read here.

`JSON_OVERLAPS` answers whether two documents share anything, and it is
sharing rather than holding: measured on 8.4.11, two arrays share an element,
two objects share a member — the same key with the same value — an array and
anything but an object share when the other is an element, and two of anything
else share when they are equal. `[[1,2]]` and `[1]` answer 0 where
`JSON_CONTAINS` of the same two answers 1. It reports a LONGLONG of 1, the
width of the one digit it writes, where `JSON_CONTAINS` reports one of 21.

`JSON_MERGE_PATCH` and `JSON_MERGE_PRESERVE` join one document into another,
and `JSON_MERGE` is MySQL's deprecated spelling of the second, still taken.
The first replaces: two objects merge member by member, a member patched with
the JSON null is taken out, and anything that is not an object replaces what it
is merged into — measured, `JSON_MERGE_PATCH('{"a":1}', '{"a":null}')` is `{}`
and `JSON_MERGE_PATCH('[1,2]', '[3]')` is `[3]`. The second keeps everything:
two arrays join end to end, two objects merge with a key held by both becoming
an array of what each held, and anything that is not an array becomes one to
join with, so `JSON_MERGE_PRESERVE('{"a":1}', '[2]')` is `[{"a": 1}, 2]`. Both
take as many documents as they are given, folded two at a time, which answers
what MySQL answers for three — measured. Both report the same JSON column the
builders do.

`JSON_ARRAY` and `JSON_OBJECT` build a document out of what they are given, and
`JSON_SET`, `JSON_INSERT`, `JSON_REPLACE` and `JSON_REMOVE` answer one with a
member changed. The engine builds and changes the same documents, so what it
answers is written again through the same canonical writer a stored document
goes through — which is what settles the three things it does differently:
MySQL's space after a comma and a colon, an object's keys sorted with the
shorter first, and a key written twice keeping the value written last.
Measured on 8.4.11: `JSON_OBJECT('bb', 1, 'a', 2, 'c', 3)` is
`{"a": 2, "c": 3, "bb": 1}` and `JSON_OBJECT('a', 1, 'a', 2)` is `{"a": 2}`.
All six report the JSON type at the widest a document can be with the text
collation and the binary flag, whatever they were given.

Each argument is a column or a plain literal. A nested call is not read, and a
boolean literal is refused: MySQL writes `true` where the engine has only the
number one to write. `JSON_OBJECT` with an odd number of arguments is refused,
where MySQL answers 1582.

A column's value goes into a built document — and into one `JSON_SET`,
`JSON_INSERT` or `JSON_REPLACE` changes — the way MySQL writes it there,
measured on 8.4.11 over every kind taken: a word or an `ENUM` member as a
string, a whole number, a `DOUBLE` and a `YEAR` as numbers, a `BIGINT
UNSIGNED` — the usual key — and a `DECIMAL` with no places as the number they
hold, `{"id": 18446744073709551615}`, a `DATE` as a string, a `DATETIME` as a
string carrying six places of a second whatever the column keeps,
`"2026-01-02 03:04:05.000000"`, and a `JSON` column as the document it holds
rather than its text. These used to answer a `DATETIME` without its places and
a `JSON` column as a string, and to refuse the unsigned key. A number written with a point keeps
the places it was written with, `JSON_ARRAY(1.5, 1.0)` being `[1.5, 1.0]`,
which the engine's double writes back the same way. Refused: a `DECIMAL` with
places, and a number written with a trailing zero past its first place or with
more than fifteen digits, which MySQL writes with every place they carry
(`1.50`, `10.00`) and a double does not; a `TIME`, and a `TIMESTAMP`, which MySQL
writes in the session's zone; and a `FLOAT`, a `SET`, a `BIT` and binary
strings, which have not been measured. A number or a moment as the document
`JSON_SET` changes is refused, which MySQL answers 3146 for.

`JSON_ARRAYAGG(JSON_OBJECT(...))` and `JSON_ARRAYAGG(JSON_ARRAY(...))` nest the
document built from each row into one array, which is how an API answer puts
a list of rows into one value; grouped, each group's rows go into its own
array. Measured on 8.4.11, it reports the JSON column the builders report and
answers NULL over no rows, and it aggregates the statement, so a bare column
beside it without a `GROUP BY` is refused as MySQL refuses it with 1140. Each
row's document is read back as a document before it goes into the array.

The four that change a document take a path naming one member of the top-level
object — `$.a`, not `$.a.b` or `$.a[0]`. That is the range the two agree on.
Measured on 8.4.11 against the engine: MySQL leaves `JSON_SET('{}', '$.x.y',
1)` alone where the engine builds the missing parent, and MySQL appends
`JSON_SET('[1,2]', '$[5]', 9)` where the engine leaves it. A one-step path
cannot reach either disagreement, so the wider paths are refused rather than
answered differently.

A member may be named in quotes, `$."city"`, which is how Laravel writes every
JSON path — `select('profile->city as city')` is `json_unquote(json_extract(
profile, '$."city"'))` and a JSON update `json_set(profile, '$."city"', ?)` —
wherever the bare `$.city` is taken, in a reading and in the four that change a
document, as long as the quoted name is letters, digits and underscores.
Measured on 8.4.11 and matched: it names the member the bare name names, in the
same column over both protocols. A reading written with `->` or `->>` over any
other path — `doc->'$.a.*'`, a quoted name holding a space — is refused: the
engine answered it, but in a column of no type at all, and `doc->'$.a.*'` over
`{"a": {"b": 1}}` answered nothing where MySQL answers `[1]`, measured.

A value bound into `JSON_SET`, `JSON_INSERT` or `JSON_REPLACE` is how Laravel
writes every JSON update — ``update `users` set `profile` =
json_set(`profile`, '$."city"', ?)`` for `update(['profile->city' => 'Kyoto'])`.
Measured on 8.4.11 with a statement prepared once and run again: a bound word
goes into the document as a JSON string whatever it holds, a document's text
and the empty word included, and NULL as the JSON null; a NULL document stays
NULL and one that is not an object is left as it was. A bound whole number goes
in as a JSON number, but from then on MySQL reads every word bound there as a
number and refuses it with 1292, so a number is refused here when it binds,
and so is a double and a binary string, which were not measured.

JSON numbers follow MySQL 8.4.11's RapidJSON conversion, including its
rounding at `1000000000000000.1` and `1e-30`. They read back as `1e15` and
`9.999999999999999e-31`, respectively.

A `WHERE` — of a `SELECT`, an `UPDATE` or a `DELETE` — takes the conditions
the frameworks write over a `JSON` column: Laravel's `where('meta->lang',
'en')`, which is `json_unquote(json_extract(meta, '$."lang"')) = ?`, its
`whereJsonContains`, `whereJsonContainsKey`, `whereJsonLength` and
`whereNull('meta->x')`, and the `meta->>'$.lang' = 'en'` Rails users write by
hand. Each reads a column of the one table the statement reads, and the
column has to be a `JSON` one. What each answers, measured on 8.4.11 over the
same rows:

- `col->>'path'` and `JSON_UNQUOTE(JSON_EXTRACT(col, 'path'))` answer text
  with the `utf8mb4_bin` collation. Against a word it tells `en` from `EN`
  and pads with spaces, so `'en  '` finds `en`; against a number both sides
  are read as doubles, the text by the number it begins with — `'1.50'`
  equals 1.5, `'true'` equals 0 and `'1abc'` equals 1. The JSON null
  unquotes to the word `null`, which is not NULL, `true` to `true`, and
  anything that is not a string is written out the way MySQL prints it, so
  `meta->>'$.tags'` is `["x", "y"]`. Every comparison operator is taken
  with a written word, a number or NULL, and `<=>` with a written one.
- `col->'path'` and `JSON_EXTRACT(col, 'path')` answer a JSON value, which
  is compared by JSON's rules, the ones a whole `JSON` column is compared by:
  `doc->'$.a' = '1'` finds the string `"1"` and not the number, a word is
  compared byte for byte with no padding, and a string ranks above every
  number, so `doc->'$.n' > 1` finds `"1"`. Django writes the value of its
  lookup as a document read out of itself — `JSON_EXTRACT(users.profile,
  '$."city"') = JSON_EXTRACT('"Paris"', '$')` — which is read as the word or
  whole number it holds, the column named through its table. A document of
  any other kind — `null` for `profile__city=None`, an object, an array, a
  double, `true` — is compared for equality with the reading, and with a
  whole column for `profile={...}`. Measured on 8.4.11: `null` finds a member
  holding the JSON null and not one that is missing, `{"a": 1}` equals
  `{"a": 1.0}` and an object with its keys in another order, `[1, 2]` does
  not equal `[2, 1]`, `true` does not equal 1, and 18446744073709551615 does
  not equal 18446744073709551615.0. Only `=` is taken with one; text that is
  no document is 3141 there and refused here.
- `reading IN (...)` and `NOT IN` — Django's `profile__city__in`, which lists
  documents read out of themselves, and SQLAlchemy's `CASE ... END IN ('a',
  'b')` — compare the reading with each member the way `=` does, measured on
  8.4.11 even where the members are of different kinds: `doc->>'$.lang' IN
  ('EN ', 5)` finds `EN` alone. A bound member is refused.
- SQLAlchemy compares a member as text through `CASE JSON_EXTRACT(col,
  'path') WHEN 'null' THEN NULL ELSE JSON_UNQUOTE(JSON_EXTRACT(col, 'path'))
  END`, naming the column through its table. Measured on 8.4.11 that is the
  unquoted text above except that the JSON null answers no value — the JSON
  string `"null"` still answers the word — and it is compared the same way,
  `IS NULL` finding both the JSON null and a missing member.
- `JSON_EXTRACT(...) IS NULL` is true only where the path is not there, a
  member holding the JSON null being found; `JSON_TYPE(...)` names it `NULL`,
  and that word is compared under `utf8mb4_bin` too, so `= 'null'` finds
  nothing.
- `JSON_CONTAINS(col, candidate[, 'path'])` and
  `JSON_CONTAINS_PATH(col, 'one' | 'all', 'path', ...)` stand on their own
  as a condition, the second also under Laravel's `ifnull(..., 0)`. A
  candidate that is not a document is error 3141 there, and refused here.
  Django writes the other two forms of the first: `JSON_CONTAINS(
  JSON_EXTRACT(col, 'path'), candidate)` for `profile__tags__contains`,
  which answers what the three-argument form answers, and
  `JSON_CONTAINS('<document>', col)` for `profile__contained_by`, which asks
  the same question the other way round. Prisma binds the path and the
  document of both: `equals` on a member is `JSON_CONTAINS(JSON_EXTRACT(col,
  ?), ?) AND JSON_CONTAINS(?, JSON_EXTRACT(col, ?))`, and `array_contains`
  the first of those beside `JSON_TYPE(...) = 'ARRAY'`. Measured on 8.4.11,
  `"Tokyo"` finds the member holding that string and neither `"tokyo"` nor a
  missing path finds anything, `["Tokyo"]` finds an array holding it, and
  `"a"`, `["a"]` and `["a", "b"]` each find `["a", "b"]`.
- `LIKE` and `NOT LIKE` over the unquoted text of a member —
  `JSON_UNQUOTE(JSON_EXTRACT(col, path))` or `col->>path`, which Prisma
  writes for `string_starts_with`, `string_ends_with` and `string_contains`
  — match under `utf8mb4_bin`, the text's collation. Measured on 8.4.11:
  `'Osa%'` finds `Osaka` and `'osa%'` does not, `'T_kyo'` finds `Tokyo` and
  `'T\_kyo'` does not, and a missing member or a NULL column matches neither
  way. The pattern is written or bound as a word; MySQL reads a bound number
  as its digits, which is refused here.
- `JSON_LENGTH(col[, 'path'])` is compared with a number. A written word is
  refused: MySQL reads `json_length(doc) = '6'` as a number.

A path is read the way MySQL reads it rather than the way the engine's own
`->` does: `$`, `.name`, `."quoted name"` — the spelling Laravel writes every
member in — and `[n]`, where a member is found only in an object and `[0]`
over anything that is not an array is the value itself, MySQL reading a lone
value as an array of one. The engine reads that as nothing. A name MySQL
refuses bare, such as `$.1a`, is refused, and so are wildcards, `last`,
ranges, spaces inside a path and a quoted name holding a quote or a
backslash.

A `?` meets these the way it meets them in MySQL: a word binds as a word and
a number as a number, `JSON_LENGTH` takes a whole number, and `JSON_CONTAINS`
looks for a document bound as text. Measured with a statement prepared once
and run again: once a number has been bound where a word is compared or a
document looked for, MySQL prepares the statement again reading that
parameter as a number and keeps reading every later word so — `'1.0'` then
equals 1, and `JSON_CONTAINS` refuses every word with 3146. From that point
the statement refuses a word here, and a number bound to `JSON_CONTAINS` is
refused outright. A prepared `UPDATE` or `DELETE` has no step holding what
binds, so a `?` against a JSON reading is refused there.

GORM's `datatypes.JSONQuery` binds the path as well —
`JSON_EXTRACT(profile, ?) = ?` for `Equals` and `... IS NOT NULL` for
`HasKey` — and compares the JSON value with a bound one, `=` and `<>` alone.
Measured on 8.4.11 with go-sql-driver's binary types: a bound word finds the
JSON string of exactly those bytes and nothing else, not a number, `true` or
the JSON null; a bound whole number finds a JSON number of that value, 30.0
included; NULL finds nothing, as does a NULL path; and the word-after-a-number
rule above holds here too. A path MySQL refuses (3143), one it reads as more
than one value, a number bound as the path (3144) and a bound double are
refused when the statement runs.

A `FOREIGN KEY` is taken and **enforced**. The engine has the enforcement and
these connections now run with it on, which is what makes taking the syntax
honest: until now the constraint was refused precisely because a stored one
would never have been checked. Measured on 8.4.11: a child row naming a
parent that is not there answers 1452 and a parent row still named by a child
answers 1451, both SQLSTATE 23000. The engine reports one failure for both
directions, so this answers 1452 either way — the direction a client meets
first. Turning enforcement on took nothing away from a table already stored
in that earlier slice: the constraint was refused at the time.

`SHOW CREATE TABLE` prints the constraint as MySQL names it, `` `t_ibfk_1` ``,
counted from one in declaration order, with its `ON DELETE` and `ON UPDATE`
where they were written. The earlier slice refused a named constraint —
`CONSTRAINT fk_parent FOREIGN KEY ...` — because the engine dropped the name.
The current frontend preserves the name. MySQL's InnoDB creates an index on
the child column and prints it — measured, `` KEY `a` (`a`) `` — and the
current frontend creates that index too.

An inline `REFERENCES` on a column is read and written nowhere, which is what
MySQL does with it. Measured on 8.4.11: `parent_id INT REFERENCES p(id)`
stores a child row naming a parent that does not exist, and `SHOW CREATE
TABLE` prints no constraint at all, whatever `ON DELETE` or `ON UPDATE` was
written beside it. The table-level `FOREIGN KEY (a) REFERENCES p(id)` is a
different statement, which MySQL does enforce, and it stays refused.

### `GROUP_CONCAT` and `group_concat_max_len`

`GROUP_CONCAT(col [SEPARATOR s])` joins a group's values the way MySQL does
and cuts the result where MySQL cuts it. Measured on 8.4.11:
`GROUP_CONCAT(name SEPARATOR '-')` answers `x-y-z`, NULLs are skipped, and a
group of nothing but NULLs, or no rows at all, answers NULL.

The cut is at the session's `group_concat_max_len` bytes, 1024 until the
session sets it. `SET [SESSION | LOCAL] group_concat_max_len = n`, the
`@@` spellings and `DEFAULT` are taken for any `n` from 4 up to the largest
unsigned 64-bit number, and `SELECT @@group_concat_max_len` and `SHOW
VARIABLES` read it back, an unsigned `LONGLONG` of 21 as in MySQL. What
MySQL does, and this server with it: the result is cut in bytes, a separator
counting like a value, and never inside a character — at 4, `aéé` becomes
`aé`, and `aé,bb` becomes `aé,`. A result exactly as long as the limit is not
cut. Each cut raises warning 1260, `Row N was cut by GROUP_CONCAT()`, where `N`
counts what that one call has joined across the statement's groups up to and
including the value the cut fell in, leaving out NULLs and the values after
a cut: over groups `x,x,x` / NULLs / `zz,zz` at 4, one call warns `Row 3` and
`Row 5`. The warnings come in the order MySQL reads the rows, two calls cut on
one row in the order they are written. A group a `HAVING` drops is still
counted and warned about, a call named again in an `ORDER BY` is not warned
about twice, and a scalar subquery counts for itself. The warnings reach the result's warning count and `SHOW WARNINGS`.
An `INSERT ... SELECT` writing a cut value fails with 1260 instead and writes
nothing, as MySQL does under the strict mode this server runs; MySQL's message
names the row, where this one's stays fixed.

`GROUP_CONCAT(col ORDER BY key [ASC | DESC] [SEPARATOR s])` joins the values
in the order of one bare column, a whole number or a word under
`utf8mb4_0900_ai_ci`, which a report of each post's tags writes. Measured on
8.4.11, a NULL the values are ordered by comes first, and last from the last,
and the cut and the row its warning names count in that order: at 4, values
ordered by `n DESC` into `b,a,A,c` answer `b,a,` and warn `Row 3`. The answer
has the shape the unordered call answers, named as written. Values tying in
the order come out in an order of MySQL's own — `a` before `A` under
`utf8mb4_0900_ai_ci` whichever way the order runs, which is not the order they
were read in — so a group whose tied values differ is refused when it is
joined, answering 1235. An order by several columns, an expression, an
ordinal, a column of another kind or words under another collation is
refused, and so is one in a subquery, where the column's kind is not read.

Over a join the call names its column with its table, and may be ordered by
that same column: the mysql client's `GROUP_CONCAT(t.name ORDER BY t.name
SEPARATOR ',')` over `users`, `posts`, `post_tag` and `tags`, grouped by `u.id,
p.id`. Such a statement is read knowing the kinds of every table's columns by
name when each name is of one kind in every table, and the column's own
collation is held to `utf8mb4_0900_ai_ci`, whatever another table's column of
that name is under. Measured on 8.4.11, it answers what the call over one
table answers — in that collation's order, from the last with `DESC`, as
numbers over whole numbers, NULL for a group the `LEFT JOIN` found nothing
for. The column shape is the unordered call's; MySQL reports the one it sorts
through when the statement's `ORDER BY` differs from its grouping, as the
next paragraph but one says. An order by another column is refused over a
join.

The engine's `group_concat` gathers each group's rows, every one written with
its length in bytes so no value can be mistaken for a separator — and, for an
ordered call, after the value it is ordered by — and `mysql_group_concat`
orders them, joins them and cuts. Numbering a cut needs what the call
joined in earlier groups, which the function leaves on the engine connection
between groups; the adapter resets it before each statement and reads the
cuts back after.

The limit also sizes the column, measured in both protocols: up to 512 a
`VAR_STRING` of four bytes to each — 16 at 4, 2048 at 512 — and past that a
`LONG_BLOB` of 64 bytes to each — 32832 at 513, 65536 at 1024, 64000000 at
1000000 — up to 4294967295. A scalar subquery answering one takes the same
shape, and `IFNULL(GROUP_CONCAT(col), 0)` the same shape, never null. A
prepared statement keeps the largest limit it was prepared or executed under,
for the cut and its column alike, which is what MySQL does over
`COM_STMT_PREPARE`: prepared at 5 and executed after the session moves to
1024 and back to 8, it answers whole and reports a `LONG_BLOB` of 65536.

Refused, each measured to differ: `DISTINCT`, which MySQL applies under the
column's collation and joins in that collation's order — over `b`, `B`, `é`,
`e`, `A` it answers `A,b,é`, where the engine keeps identical values apart
from the others and joins them in the order it read them; a
`DOUBLE` or `FLOAT` column, which MySQL writes the shortest way (`1e20`,
`0.1`) and the engine does not (`1.0e+20`, `0.100000001490116`); a `BLOB`, a
binary string and a `JSON` column, each answering a binary result of a width
of its own; a `GROUP_CONCAT` in a `UNION` branch, whose column MySQL reports
by a rule of its own; a `GROUP_CONCAT` in an `ORDER BY` that the projection
does not name, which MySQL cuts and warns about all the same; and a limit
below 4, which MySQL takes as 4 with warning
1292.
MySQL answers 1232 for a limit written as a word, a number with a point,
`NULL` or `ON`, and those are refused. A prepared `GROUP_CONCAT` answers its
bytes length-encoded, as MySQL does.

`JSON_ARRAYAGG(col)` collects a column of words or of whole numbers into a
JSON array, in the order the rows are read, the way `GROUP_CONCAT` joins them.
Measured on 8.4.11: a word becomes a JSON string escaped the way MySQL escapes
one (`["a\"b", "c\\d", "e\nf\tg"]`), a `CHAR` without its trailing spaces, a
whole number a JSON number up to the largest `BIGINT`, a NULL the JSON null,
and no rows at all answer NULL rather than an empty array. It reports the JSON
type at the widest a document can be, with the text collation and the binary
flag, as `JSON_ARRAY` does. The engine's `json_group_array` builds the array,
the document is written again the way MySQL writes one, and an empty group is
answered NULL. Refused: a `DOUBLE`, a `DECIMAL`, a moment or a JSON column,
each of which MySQL writes into the array by a rule of its own, a `DISTINCT`,
and `JSON_OBJECTAGG`, which MySQL answers 3158 for a NULL key.

`STDDEV_SAMP` is taken, and it is the only standard deviation that is.
Measured on 8.4.11 over 2, 4, 4, 4, 5, 5, 7, 9: the sample form answers
2.138089935299395 and MySQL's `STDDEV`, `STD` and `STDDEV_POP` answer 2, the
population form. The engine has one deviation aggregate and it is the sample
one, so the population spellings are refused rather than answered with a
different number. The variance family is refused for a related reason: a
variance is the square of a deviation, and squaring a rounded square root
would answer different last digits than MySQL's own. The result is a `DOUBLE`
of length 23 with the not-fixed decimals value, and it is nullable — one row
has no sample deviation, which both engines answer NULL for.

`FLUSH TABLES` is answered with an OK. MySQL closes its table cache there, and
this server keeps no table cache, so the statement asks for something already
true — the one shape of `FLUSH` that can be answered without promising
anything. `FLUSH TABLES WITH READ LOCK` holds a lock across statements,
`FLUSH PRIVILEGES` reloads grants this server reloads on its own schedule, and
`FLUSH LOGS` rotates logs it does not keep; each is refused rather than
answered with an OK it would not keep. A table list is refused too: it names
tables to close, and there is no cache holding them.

`SHOW WARNINGS` reports what the last statement raised. This server raises one
warning, the note a `DROP TABLE IF EXISTS` leaves when the table is not there,
and it is the one MySQL raises: measured on 8.4.11, `Note`, code 1051, and a
message naming the table with its database. Every other statement answers the
columns and no row, which is what MySQL does when nothing has warned. Reading
them does not clear them, and the next statement that can raise one does.

The columns are measured: `Level` is a `VAR_STRING` of length 28, `Code` a
`LONG` of length 5 carrying the unsigned, binary and numeric flags, and
`Message` a `VAR_STRING` of length 2048; all three are NOT NULL. `LIMIT` restricts
the count and takes an optional offset, matching MySQL's `LIMIT [offset,] row_count`
and `LIMIT row_count OFFSET offset`. `SHOW ERRORS` shares the same column shape
and answers only the diagnostics with level `Error`. `SHOW COUNT(*) WARNINGS` and
`SHOW COUNT(*) ERRORS` return a single `LONGLONG` column (`@@session.warning_count`
or `@@session.error_count`) reporting the count of warnings or errors from the
previous statement without clearing them.

`SELECT @@name` answers the variables this server has an honest answer for and refuses the
rest, which is the same rule `SHOW VARIABLES` follows. Every client opens by reading a handful
of them, so refusing them all ends a connection before any work starts. Taken: `@@version` and
`@@version_comment`, `@@sql_mode`, `@@autocommit`, `@@sql_notes`, `@@foreign_key_checks`, `@@unique_checks`,
`@@max_allowed_packet` and `@@wait_timeout`, the five `@@character_set_*` names, the three
`@@collation_*` names, `@@system_time_zone` and `@@time_zone`, `@@transaction_isolation`,
`@@auto_increment_increment` and `@@auto_increment_offset`, `@@interactive_timeout`,
`@@performance_schema`, `@@lower_case_table_names`, `@@init_connect`, `@@license`, `@@socket`,
`@@net_read_timeout`, `@@net_write_timeout` and `@@terminology_use_previous` — one at a
time or a row of them at once, under any scope and under an alias — `SELECT @@sql_notes AS n` and `SELECT @@global.sql_notes` are read
now, where the switch once had a reader of its own that took one bare spelling. A driver opens the connection by reading a row of them — the pinned
`mysql_async` one sends `SELECT @@max_allowed_packet,@@wait_timeout` — so a list is read rather
than only one name, and each column is the one that variable answers on its own, in the order
the statement names them. A name this server has no answer for fails the whole statement rather
than leaving a column out, and it fails as 1193 with SQLSTATE HY000 — what
MySQL answers for a variable no build of it has, measured on 8.4.11 — rather
than as a refusal of the statement's shape. MySQL writes the first unknown
name into the message and this server does not, the way every other message
here is the short one.

Each name past `@@version` is something this server decides for itself rather
than a default copied from MySQL, which is what makes it worth answering. It
speaks `utf8mb4` and refuses any other character set but latin1 below, so all
five `@@character_set_*` names read `utf8mb4` unless the session named latin1.
The greeting names collation 45, so `@@collation_connection` reads
`utf8mb4_general_ci` for a client whose handshake names it, or a utf8mb4
collation this server does not keep, until the session names another; a
handshake naming `utf8mb4_0900_ai_ci` or `utf8mb4_unicode_ci` reads it back,
as in MySQL. A table written here is
declared the way MySQL declares one, so `@@collation_server` reads
`utf8mb4_0900_ai_ci` — which is what every `SHOW CREATE TABLE` and
`information_schema` reading here already says — and `@@collation_database`
the selected database's collation, described under **A database's
collation**. The server
uses UTC for its own clock, so `@@system_time_zone` reads `UTC`.
`@@transaction_isolation` reads the level the session runs at, `REPEATABLE-READ`
until the session asks for `READ COMMITTED`. The counter numbers from one
and steps by one, so both `@@auto_increment_*` read 1. A connection a client
called interactive is kept no longer than any other, so `@@interactive_timeout`
reads what `@@wait_timeout` does. Nothing runs when a connection opens, so
`@@init_connect` is empty.

Three of them read differently from MySQL's own answer, because this is not
MySQL. `@@performance_schema` reads 0 where MySQL reads 1: there is no
performance schema here, and a client can see that for itself. `@@license`
reads `MIT`, the licence this repository carries, where MySQL reads `GPL`.
`@@lower_case_table_names` reads 1 where MySQL on Linux reads 0: a table
written as `Users` here is found again as `users` and reads back lowercased
from `SHOW TABLES`, measured against this server, and 1 is what MySQL calls
that.

`@@socket` names the Unix socket this server listens on. Prisma's driver,
`mysql_async`, reads it with `@@max_allowed_packet` and `@@wait_timeout` on
every connection and, when it names a path, opens a second connection there and
keeps that one. A server listening on TCP here listens on no socket — the two
listeners are never configured together — so it reads an empty path, in the
same `VAR_STRING` of 87380 with 31 decimals and no flags a set one answers in,
and `SHOW VARIABLES` writes it empty too. MySQL 8.4.11 reads a path left unset
as NULL (`@@init_file`, measured), but a MySQL server always has a socket path,
and the driver converts `@@socket` to a string without looking: a NULL made
Prisma panic on every connection, in the framework harness. An empty path is
one the driver fails to open, which it ignores, and it stays on TCP. `@@SESSION.socket` is refused, where
MySQL answers 1238, and `SET @s = @@socket` with no socket is refused, since
MySQL gives the variable a NULL of words there that this does not keep apart.

`mysqldump` 8.4 opens every dump with `SET SESSION NET_READ_TIMEOUT= 86400,
SESSION NET_WRITE_TIMEOUT= 86400`. Both take one second to a year and read back
as set, as unsigned `LONGLONG`s of 21, measured on 8.4.11; a value outside that
is refused where MySQL clamps it with warning 1292. `net_write_timeout` is this
server's own write deadline: a session that sets it has each answer after that
given that long to be written, in place of the runtime's configured deadline,
and `DEFAULT` gives the configured one back. `net_read_timeout` is kept and read
back — 30, MySQL's default, until a session sets it — but nothing here waits by
it: MySQL uses it only to give up on a client that stops in the middle of
sending a command, and this server waits for the rest of a command until the
idle deadline, as it waits for its first byte, so the value changes no answer.
`mysqldump` also sends `SET @@SESSION.terminology_use_previous = NONE` before it
lists routines or events. It chooses between the old and the new words for
replication in what `SHOW` prints, and this server prints neither, so `NONE` —
what a session starts with, and what 0 and `DEFAULT` also name, measured — is
taken and reads back, and `BEFORE_8_0_26` is refused.

Django reads `CONVERT_TZ('2001-01-01 01:00:00', 'UTC', 'UTC') IS NOT NULL` beside the
variables when it connects, to learn whether the server has named zones. It answers 1 when
both zones are ones this server knows — `UTC`, `SYSTEM` and fixed offsets — and 0 for a named
zone, which is what a MySQL whose zone tables are empty answers; the pinned oracle has them
loaded and answers 1 for named zones too.

`@@time_zone` reads back the zone the session last named. `UTC` and `SYSTEM`
mean UTC; fixed offsets affect supported `TIMESTAMP` reads and writes.
Measured on 8.4.11 and matched: `SYSTEM` is a
keyword and reads back upper-cased whatever case it was written in, and an
offset reads back as `+HH:MM`, so `'-00:00'` reads `+00:00`. A session that has
named none starts at `SYSTEM`, MySQL's own default, and `@@global.time_zone`
stays there whatever the session named.

A named zone reads back as the statement wrote it, which is the one place this
deliberately does not follow a running MySQL. MySQL keeps the first spelling a
whole *server* saw for a zone name and answers that to every session
afterwards: measured on two servers of the same pinned image, one sent
`SET time_zone = 'UTC'` first and answers `UTC` to a later `'utc'`, while one
sent `'utc'` first answers `utc` to both. That is the server's history rather
than a rule about the zone, and this server keeps no such history — it answers
what the session wrote, which is what a MySQL that has not seen the name before
answers. A scope decides
which value answers: `@@name`, `@@session.name` and `@@local.name` read what this session is
using, and `@@global.name` reads what a new session would start from, since nothing here can
change a global value. Measured on 8.4.11: a session that turns `autocommit` and
`foreign_key_checks` off and adds `ANSI_QUOTES` to its `sql_mode` reads all three back unchanged
under `@@global.`. `@@sql_mode` answers the modes MySQL's own default names, which are the ones
this enforces, with `ANSI_QUOTES` and `NO_BACKSLASH_ESCAPES` added when the session was opened
with them, and `STRICT_ALL_TABLES` and `NO_AUTO_VALUE_ON_ZERO` when the session named them.
MySQL writes the modes in an order of its own rather than the order they were set in, measured,
and so does this. `@@wait_timeout` reads the idle time the session asked for, and
`@@global.wait_timeout` the server's own. `@@sql_auto_is_null` and `@@sql_safe_updates` read 0,
and `@@default_storage_engine` reads `InnoDB`.

Their shapes are measured on 8.4.11: a word answers the same `VAR_STRING` of length 87380 with
31 decimals and no flags that `@@version` does; `@@autocommit`, `@@sql_notes` and
`@@foreign_key_checks`, `@@unique_checks`, `@@sql_auto_is_null` and `@@sql_safe_updates` answer a `LONGLONG` of length 1 carrying the binary and numeric flags; and `@@max_allowed_packet` and
`@@wait_timeout` answer a `LONGLONG` of length 21 carrying those and the unsigned flag. The
two counters answer this server's own values rather than MySQL's defaults, which is what makes
them honest.

`SET foreign_key_checks = 0` says a row this session writes need not name a
parent that is there. It is the first thing a fixture loader says and the first
thing a dumped schema says, both of them writing rows in an order no foreign key
would allow, so refusing it stopped the load at its first statement. Unlike the
other settings this takes, it is not a restatement of what the server already
does: the engine has the same switch, and it is turned. A standard dump turns
it off before its `CREATE DATABASE` and `USE`, so a connection `USE` or
`COM_INIT_DB` opens is given the switch, the lock wait and the zone the session
set before it had one; without that, a dump's rows for a child table, written
before its parent in name order, were refused.

Measured on 8.4.11 and matched: the switch reads 1 to begin with, a child row
pointing nowhere is refused while it is on, turning it off lets that row in,
turning it back on leaves the row where it is rather than looking at it again,
and the next row pointing nowhere is refused again. `OFF` and `ON` say what 0
and 1 say and read back as them. A value that is neither is refused, where MySQL
answers 1231.

`SET unique_checks` takes 0, 1, `OFF` and `ON`, and `@@unique_checks` reads it
back. It changes nothing here, which is what it changes in MySQL 8.4 as
configured by default: InnoDB skips a uniqueness check only for a row it
buffers in the change buffer, and `innodb_change_buffering` is `none` from 8.4
on. Measured on 8.4.11: with it off, a second row with a key already there is
refused with 1062, into a unique secondary index and into the primary key
alike.

`CREATE TABLE IF NOT EXISTS` is what an idempotent setup script writes, and it
was refused outright. MySQL leaves a table that is already there exactly as it
stands and raises note 1050, whatever the rest of the statement says, so the
name is looked up before anything runs rather than left to the engine's own
guard — which would say nothing about what it skipped.

Measured on 8.4.11 and matched: the words are not printed back, a second
statement naming another column changes nothing and warns once with
`Table 't' already exists`, and a table that counts its own ids keeps its rows
and its counter through one. A view of the same name counts as a table that is
already there, which is what MySQL's 1050 counts.

Written without the words, a table that is already there is the error MySQL
answers, 1050 with SQLSTATE 42S01 — it used to be refused as unsupported. The
same lookup answers both, so a `CREATE TABLE` naming a table, a view, a counted
table or one written from a `SELECT` all answer it alike. A `CREATE TEMPORARY
TABLE` is left alone: MySQL lets a temporary table stand beside a permanent one
of the same name and shadow it, so a name already taken says nothing about one.

`DEFAULT CURRENT_TIMESTAMP` is on nearly every table a dumped schema carries —
the `created_at` column an ORM writes — and was refused as a non-literal default.
The engine spells the moment a statement runs at in UTC. That is the value a
`TIMESTAMP` stores; a non-UTC `DATETIME` insert taking this default is refused
because it would need the session's wall time instead.

Measured on 8.4.11 and matched: `NOW()` and `CURRENT_TIMESTAMP` are the same
default and both print back as `CURRENT_TIMESTAMP`; `SHOW COLUMNS` and
`information_schema.COLUMNS` report it as `CURRENT_TIMESTAMP` with an extra of
`DEFAULT_GENERATED`; `SHOW CREATE TABLE` prints the default and no extra; and a
row taking it is written the moment it lands. The default is taken on a
`TIMESTAMP` and a `DATETIME` and no other column — measured, MySQL answers 1067
for one on an `INT`.

`DEFAULT (now())` — the expression default SQLAlchemy writes for
`server_default=func.now()`, which its first Alembic migration declares — is
taken over a whole-second `DATETIME` or `TIMESTAMP`, and so are `(NOW())`,
`(current_timestamp)` and `(current_timestamp())`. Measured on 8.4.11 and
matched: each stores the moment the row is written, in whole seconds, and prints
back as `DEFAULT (now())`, where `DEFAULT now()` without its parentheses prints
as `DEFAULT CURRENT_TIMESTAMP`; `SHOW COLUMNS` and `COLUMN_DEFAULT` read `now()`
with an extra of `DEFAULT_GENERATED`. The engine's reading of the moment is
written in parentheses, which is what says it was an expression. A column
holding fractional seconds or a day, `(now(3))`, `(curdate())`,
`(localtime())` and every other expression default are refused, and a
`CREATE TABLE ... AS SELECT` copying such a column is refused, what either
writes not having been measured.

`ON UPDATE CURRENT_TIMESTAMP` is the other half of the pair every dumped schema
carries, and the whole clause was refused. The engine has no such attribute, so
the words live in the stored MySQL DDL alone — read back out of it before the
engine's own parser sees the rest, the way the `AUTO_INCREMENT` marker is — and
what they mean is written into each `UPDATE` instead: the column takes the
moment under a condition asking whether any column the statement assigns is
about to move, which the engine reads against the row as it stands.

Measured on 8.4.11 and matched: an `UPDATE` that changes the row rewrites the
column, one that changes nothing leaves it and counts no row, and one that names
the column writes what it says. An `INSERT` leaves it as written — the clause
speaks only of updates. `SHOW CREATE TABLE` prints the words after the DEFAULT
clause, and `SHOW COLUMNS` and `information_schema.COLUMNS` report `on update
CURRENT_TIMESTAMP`, or `DEFAULT_GENERATED on update CURRENT_TIMESTAMP` where the
column takes the moment as its default too.

A value the statement writes is written twice — once as the assignment and once
in the condition — so a bound `?` in one is named by its ordinal in the other; a
prepared `UPDATE` asks for the parameters the statement wrote and no more. Each
`?` of the `SET` is counted, so `SET a = ?, b = ?` compares the second with `b`
— it used to compare every one with the first. A
joined `UPDATE` on such a table is refused: it names the table it changes through
the columns its `SET` names, so which table's columns are its own is a question
this has not answered.

Only the keyed `CREATE TABLE` paths render the stored MySQL DDL from the
statement as written, so only they can keep the words. A table with no key of its
own, and every `ALTER TABLE` — `ADD COLUMN`, `MODIFY COLUMN`, `CHANGE COLUMN` —
rebuild that DDL from the engine's own definition, where the attribute is not.
Each of those refuses the statement rather than taking it and printing a table
without the words, which would be a different table than the one asked for.

A column `COMMENT` says what the column holds, and a dumped schema written by
anyone who annotates their tables carries one on nearly every column, so the
whole `CREATE TABLE` was refused for it. It is taken now, and lives in the stored
MySQL DDL the same way, under the same limit: the keyed `CREATE TABLE` paths keep
it and the paths that rebuild the DDL from the engine's definition refuse it.

The text is written back the way MySQL's own `SHOW CREATE TABLE` writes it,
measured on 8.4.11 by reading the printed bytes: a quote is doubled, a backslash
is written twice, a newline becomes `\n`, a carriage return `\r` and a zero byte
`\0`. Everything else is printed as it stands — a tab, a double quote, a `%` and
a `_` each come back raw, and so does 0x1A, which is why `\Z` is not written.
The words are printed last, after `AUTO_INCREMENT`, after `PRIMARY KEY` and after
`ON UPDATE CURRENT_TIMESTAMP`, and an empty comment is not printed at all.

`SHOW FULL COLUMNS` reports the text in its `Comment` column and `SHOW COLUMNS`
does not report it, both as MySQL does. `information_schema.COLUMNS` answers it
under `COLUMN_COMMENT`, a blob of 24576 that is never null — a column with no
comment answers the empty string, not NULL — which is a column a query may now
name.

`CAST(col AS CHAR)` asks for a column's value spelled out, and it answered only
a whole number, a day and a moment. Every kind whose spelling the engine writes
out the way MySQL does is taken now — the narrower and wider integers, a `TIME`,
a `YEAR`, and a `VARCHAR` or `CHAR`, which spells itself.

Measured on 8.4.11 and matched: the answer is a `VAR_STRING` in utf8mb4,
nullable, with no flags, 31 decimals, and a width four times what the column can
spell — an `INT` of eleven characters answers 44, a `SMALLINT` 24, a `MEDIUMINT`
36, a `BIGINT` 80, a `TINYINT(1)` and a `YEAR` 16, a `DATETIME` 76, a `DATE` and
a `TIME` 40, a `VARCHAR(20)` 80 and a `CHAR(4)` 16. The 31 decimals were
reported as 0 before this, which was wrong and is measured now.

A `DECIMAL`, a `FLOAT` and a `DOUBLE` stay refused: the generic text conversion
has not been verified against MySQL's numeric formatting, even though a direct
`DECIMAL` result now keeps its scale. A `TEXT` is refused as well; MySQL answers a `MEDIUM_BLOB` of
1048560 for one, a different shape that has not been implemented.

A `WHERE` comparing a column that holds a moment against a written day reads the
day as that day's midnight, which is what MySQL reads it as. It is the shape
nearly every test's date filter has — `created_at > '2026-01-01'` — and it was
refused, because reading the two as text answers a different set of rows: a
`DATETIME` is held as `2026-01-05 10:00:00` where a day is ten characters, so a
row standing exactly at a day's midnight reads as later than that day rather
than the same as it.

Only the frontend can see which kind a column is, so a statement writing a day
says so and is rendered a second time knowing, the same way one ordering a text
column is. Measured on 8.4.11 and matched, over a row at `2026-01-05 10:00:00`
and one at `2026-02-01 00:00:00`: `> '2026-01-01'` finds both, `> '2026-02-01'`
finds neither, `>= '2026-02-01'` and `= '2026-02-01'` find the midnight row,
`= '2026-01-05'` finds none, and `BETWEEN` two written days finds the one inside
them. A `DATE` column reads the same written day as itself, which is the form it
holds, and a written moment reads as itself everywhere. An `UPDATE` and a
`DELETE` refuse it: neither has a second rendering pass to learn what the column
holds.

A value bound against one of those columns reads the same way. `WHERE
created_at > ?` is what an ORM binds for every date filter it runs, and it was
refused: a bound value carries no type until it binds, and nothing put it into
the form the column holds. It is put into that form now, by the caller that
knows which column the parameter meets, and the refusal is lifted for exactly
the parameters that get it — finding a comparison's column stopped judging the
comparison, so the `SELECT` path takes what the DML path, which has no such
step, still refuses.

Measured on 8.4.11 and matched, over the same two rows: bound `'2026-01-01'`
finds both, bound `'2026-02-01'` finds neither with `>` and the midnight row
with `>=` and `=`, a bound moment reads as itself, a loosely written `'2026-1-5'`
is read as that day, and a word that reads as no moment at all finds no row —
MySQL warns 1292 about that one as well, and this does not. A bound NULL finds
no row, the way a comparison against one does.

A bound number is refused: MySQL reads one as a moment — 20260101000000 names
the first of January — and what this reads is a word.

A driver that holds a date as a value of its own sends it as a binary parameter
rather than as a word — JDBC does, where a driver holding it as text sends a
string — and that parameter type was refused outright. The bytes are read into
the word MySQL would have read, so both spellings meet the column through the
one reader and find the same rows. MySQL writes a day as four bytes and a moment
as seven, after a length; a length of 11 adds microseconds and is accepted for
`DATETIME` and `TIMESTAMP`. A length of 0 names the zero date, which the
sql_mode this server runs in refuses. A `MYSQL_TYPE_DATE` carrying a time of
day is refused as well, a `DATE` naming none. A `MYSQL_TYPE_TIME` parameter
uses its own signed span encoding, including its 12-byte microsecond form.

`NULLIF(a, b)` answers its first argument, or NULL where the two match, which
is how a statement guards a division against the value that would make it
meaningless. Only the form comparing a column against a written number was
taken; two columns are compared now as well.

Measured on 8.4.11 and matched: the answer is the *first* argument's shape
whatever the second is — a `SMALLINT` first and a `BIGINT` second answers a
`SHORT` of 6, and the other way round a `LONGLONG` of 20 — and it is always
nullable. Two columns of words are refused, and so is a number against a word:
MySQL compares those by its own rules, reading a word as a number and comparing
two words without regard to case, where the engine compares them by their kinds.

`SHOW VARIABLES` reports the system variables this server actually has — the
same twenty-six names `SELECT @@name` answers, read through the same two
readers so the two cannot drift apart — in the name order MySQL writes them in,
rendered the way `SHOW VARIABLES` renders them. Measured on 8.4.11: a switch
reads `ON` or `OFF` where `SELECT @@name` answers 1 or 0, so `sql_notes`,
`autocommit`, `foreign_key_checks` and `performance_schema` read as words while
`lower_case_table_names` and `interactive_timeout` read as numbers; a switch is
exactly a variable whose column is one digit wide, which is how the two are
told apart here. MySQL 8.4.11 returns 647 rows here. Any other name returns the two columns and no row, which is
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

A value the validator refuses fails its statement and nothing else. Measured on
MySQL 8.4.11 inside a transaction: a value too long (1406), out of range (1264),
not a number (1366), not a moment (1292), not a member (1265) or not a document
(3140) each leaves the transaction open with its earlier rows and its
savepoints; a multi-row `INSERT` refused on its second row keeps none of its
rows, and an `UPDATE` refused on a later row leaves the rows before it as they
were. So it is here. The engine checks a row just before writing it, after the
row's index entries and any earlier rows are in, so every write under the
validator takes a statement savepoint inside a transaction and a refusal rolls
back to it, the way a failed `CHECK` or `NOT NULL` does.

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

`TINYINT UNSIGNED`, `SMALLINT UNSIGNED`, `MEDIUMINT UNSIGNED` and
`INT UNSIGNED` are taken. The sign is kept as part of the declared type name —
the stored DDL says `INT UNSIGNED`, not `INT` with a flag beside it — which is
what carries it to `SHOW CREATE TABLE`, to `SHOW COLUMNS`, and to the result
metadata. Measured on 8.4.11: each reports the wire type its signed counterpart
reports, one digit narrower, with the UNSIGNED flag —
`TINYINT UNSIGNED` type 1 length 3, `SMALLINT UNSIGNED` type 2 length 5,
`MEDIUMINT UNSIGNED` type 9 length 8 and `INT UNSIGNED` type 3 length 10,
against 4, 6, 9 and 11 for the signed ones, because an unsigned column spends
no character on a sign. `SHOW CREATE TABLE` and `SHOW COLUMNS` print the sign as
a second lower-case word, `int unsigned`. The stored range is checked before a
value is written, so 255, 65535, 16777215 and 4294967295 are the top values each
accepts and one past any of them is refused, as is a negative — MySQL answers
1264 for both.

`DOUBLE UNSIGNED`, `FLOAT UNSIGNED` and `DECIMAL(p,s) UNSIGNED` are taken as
well, and the sign means the same thing there: what may be written. Measured on
8.4.11, a negative answers 1264 in any of them while zero is taken, and each
reports its signed form's type with the unsigned flag beside it. A `DOUBLE`
reports 22 and a `FLOAT` 12, both with the not-fixed decimals value and neither
narrower than its signed form, because a binary float spends no character on a
sign. A `DECIMAL` does spend one, so an unsigned one is a digit narrower
throughout: `(10,2)` reports 11 against 12, `(5,0)` 5 against 6, `(65,30)` 66
against 67 and `(1,1)` 2 against 3. The sign prints as a second lower-case word
after the arguments, `decimal(10,2) unsigned`.

The engine stores signed and unsigned decimal columns under internal type names,
`mysql_decimal(p,s)` and `mysql_decimal_unsigned(p,s)`. Schema output and result
metadata still use MySQL's `decimal(p,s)` spelling and unsigned flag.

`SHOW CREATE TABLE` prints an unsigned integer the way MySQL does, the sign a
second lower-case word after the type — `tinyint unsigned`, `smallint
unsigned`, `mediumint unsigned`, `int unsigned`, `bigint unsigned`, all
measured on 8.4.11, with `INTEGER UNSIGNED` printing as `int unsigned` the way
plain `INTEGER` prints as `int`. Before this the statement refused any table
carrying one.

`BIGINT NOT NULL AUTO_INCREMENT` is taken, which is the key an ORM's migration
writes by default — Rails and Laravel both write it — so refusing it refused the
first table of most schemas. It counts the way an `INT` does and stops where the
engine does, at 9223372036854775807. Measured on 8.4.11 and matched: it prints
back as `bigint`, reports `bigint` and `auto_increment` in its columns, counts
from one, and carries on past a written id no `INT` could hold. A display width
is dropped as it is on any other integer column. `BIGINT UNSIGNED AUTO_INCREMENT`
uses a separate `mysql_uint64` primary key and a durable unsigned counter, so
generated ids can pass `i64::MAX`. A start above that boundary, multiple
generated rows, explicit wide ids, and reopening are covered. `ON DUPLICATE
KEY UPDATE` is refused on this table shape until the updated row's id can be
reported correctly.

`INT UNSIGNED AUTO_INCREMENT PRIMARY KEY` is taken, which is the spelling a
MySQL schema usually gives a surrogate key. The allocator counts in an i64 and
4294967295 fits one, so nothing about the numbering changes. How high it may
count is the column's own type rather than a fixed ceiling: an `INT` stops at
2147483647 and an `INT UNSIGNED` at 4294967295, so an `UPDATE` that moves the
counter to 3000000000 is taken on the second and refused on the first.

`BIGINT UNSIGNED` stores the full range 0..18446744073709551615. The MySQL
frontend declares an internal `mysql_uint64` type whose sortable nine-byte
blob keeps values above `i64::MAX` exact. It decodes to decimal text; a result
column reports LONGLONG, length 20 and UNSIGNED, and a prepared binary result
carries the full `u64` value. Inserting and reading the boundary values,
ordering by the column, indexed equality and range comparisons, prepared
unsigned parameters, and reopening the database are covered. MySQL 8.4.11
accepts the same endpoints and rejects negative assignments with 1264, as
this frontend does. An assignment above `u64::MAX` is also refused. A prepared
`UPDATE` or `DELETE` finding its row by a `BIGINT UNSIGNED` column compared with a
bound value — Eloquent's `where id = ?` over every key Laravel makes, and a
pivot's `post_id = ? and tag_id in (?)` — compares it the way a prepared
`SELECT` does; it used to match no row, the engine comparing the bound number
with the column's stored form.

GORM's statements of the same kind are covered too: `UPDATE posts SET
views=views + ? WHERE user_id = ?`, `WHERE id > ?`, `BETWEEN ? AND ?`, a
detach's `post_id = ? AND tag_id NOT IN (?,?)` and a written `id IN (1, 2)`,
each of which used to change no row or every row; measured on 8.4.11, each
finds the rows whose ids compare as numbers.

A `SELECT` over several tables was read without its columns' types, so it
compared such a column by kind too: GORM's count of an association through its
join table — `JOIN post_tags ON post_tags.tag_id = tags.id AND
post_tags.post_id = ?` — found no row. It is now read knowing which of the
columns it names are `BIGINT UNSIGNED` or `DECIMAL` — a `DECIMAL` compared
with a bound value in a join was compared by kind the same way,
`users.balance > ?` finding every row binding 50 and none binding `'50'` — and
refused where that name is also a column of another kind. One reading a
subquery beside such a comparison, which is read without its columns' types,
is refused. The same second reading knows which names are columns of words, so
a bound word meets one in a join under the column's own collation — GORM's
`Joins("JOIN emails ON emails.user_id = users.id AND emails.email = ?", ...)`,
which was refused — without regard to case and with a trailing space
significant, as measured on 8.4.11; a name that is words in one table and
another kind in another is refused.

This internal type is created only for the MySQL frontend. An older MySQL
table declared as `BIGINT UNSIGNED` used signed integer storage; its original
high values cannot be recovered from that representation, so opening it now
fails with a re-import instruction. Ordinary SQLite integer columns keep
their existing signed storage.

Some expressions still stop before they can round a wide unsigned value. A
prepared unsigned value above `i64::MAX` compared with a signed `BIGINT` is
refused with 1235/42000; MySQL 8.4.11 returns no matching row. An untyped
prepared projection with that value is refused rather than sent through the
engine's approximate numeric conversion.

`BOOLEAN` and `BOOL` are taken as what MySQL makes them: a `TINYINT` carrying
the display width one. `SHOW CREATE TABLE` and `SHOW COLUMNS` print
`tinyint(1)` for either spelling, and a result column reports the TINYINT type
with length 1, where a plain `TINYINT` reports 4 — measured. The value is a
`TINYINT`'s and is held to a `TINYINT`'s range, so 999 is refused.

`DATETIME` holds MySQL's own text form, and takes the wide
input surface MySQL takes: measured on 8.4.11, `'2026-9-6 1:2:3'`,
`'2026-09-06'`, `'20260906010203'` and `'2026-09-06T01:02:03'` are all read and
stored as `YYYY-MM-DD HH:MM:SS` at the default precision zero, and
`'...01:02:03.5'` rounds up to the next second, carrying into the next day
and the next year where it has to. A declared precision from 1 through 6
stores that many fractional digits. Input rounds to microseconds first and then
to the column's precision, matching MySQL 8.4.11 at boundaries such as
`.1249995` in `DATETIME(2)` becoming `.13`.

A column holding fractional seconds may default to, and be rewritten on update
to, the moment at its own precision — `DATETIME(3) DEFAULT
CURRENT_TIMESTAMP(3)`, which Prisma writes for every `@default(now())`, and
TypeORM's `datetime(6) ... DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE
CURRENT_TIMESTAMP(6)`. Measured on 8.4.11 and matched: another count of digits
than the column holds is 1067 for the default and 1294 for `ON UPDATE`; and
`SHOW CREATE TABLE`, `SHOW COLUMNS` and `information_schema.COLUMNS` name the
digits — `CURRENT_TIMESTAMP(3)`, `on update CURRENT_TIMESTAMP(6)`. One thing
differs: the engine's clock reads to the millisecond, so a fourth digit and
beyond are zeros where MySQL's clock fills them, and fewer digits are cut off
rather than rounded, since rounding a clock reading up names a moment that has
not come yet.

Which of MySQL's two readings applies turns on the character right after the
leading run of digits, which is worth knowing because it changes what the year
is. A run that ends the value or is followed by a point is read as if the whole
value had been written without separators: the run is cut into a year and then
two digits at a time, and the year is four digits only when the run is four,
eight or fourteen long. So `'0.1.1'` is the year 2000 where `'0-1-1'` is the
year 0, and `'3311309'` is 2033-11-30 with an hour of 9 left over. Measured
too: the year zero is not a leap year here, where the usual rule would make it
one.

The calendar is checked the way MySQL checks it: `'2026-02-30 00:00:00'` is
1292 there and is refused here too, leap years included. `SHOW CREATE TABLE`
and `SHOW COLUMNS` print `datetime` or `datetime(fsp)`, and a result column
reports type 12 with length 19 plus the fractional point and digits when fsp is
nonzero, decimals equal to fsp, and the binary flag, because a temporal column
carries no collation — measured. Precision above 6 is refused.

`DECIMAL(p,s)` stores an exact decimal blob. Text and prepared writes, defaults,
comparisons, indexed ordering, `SUM` and `AVG` keep the digits through a reopen.
Assignment rounds half away from zero to the declared scale and rejects a value
outside the declared precision. Measured on MySQL 8.4.11 and matched: `12.345`
into `DECIMAL(5,2)` reads back as `12.35`; three rows holding
`0.100000000000000000000000000001` sum to
`0.300000000000000000000000000003`. `AVG` adds four decimal places up to
MySQL's maximum scale of 30. The text and prepared protocols write the fixed
scale as text, so a `DECIMAL(10,2)` holding 1.5 reads back as `1.50`.
`UPDATE` arithmetic with a DECIMAL operand or target keeps fractional literals
and bound text values exact until assignment. The same applies to arithmetic
in `ON DUPLICATE KEY UPDATE` when its target is DECIMAL, including nested
expressions. Division by a written nonzero integer uses decimal arithmetic.
An `UPDATE` that divides an untyped column into a DECIMAL
target is refused until its source type can be checked. Prepared DML is refused
after the table definition changes, so a previous DECIMAL scale is never reused.

An existing table declared with the old binary64 `DECIMAL` type cannot recover
its original digits. Opening it through the MySQL frontend fails with a
migration/re-import error. Recreate the table with the exact type and re-import
the original decimal values.

Everything a client reads *about* a `DECIMAL` column does match. `SHOW CREATE
TABLE` and `SHOW COLUMNS` print `decimal(10,2)`, a bare `DECIMAL` means
`DECIMAL(10,0)` and prints as such, and a result column reports `NEWDECIMAL`
with the scale as its decimals and a length of the precision, plus one for the
sign, plus one more for the point when the scale is above zero. That rule was
derived from six measured shapes and holds for all of them: 12 for (10,2), 6 for
(5,0), 67 for (65,30), 11 for (10,0), 3 for (1,1), 22 for (20,4). MySQL's own
bounds hold too: a precision past 65, a scale past 30, a scale wider than its
precision and a zero precision are all refused.

`TIMESTAMP` stores a UTC instant. Measured on MySQL 8.4.11, one row reads back
as `2026-09-06 01:02:03` under `+00:00` and `2026-09-06 10:02:03` under
`+09:00`, while a `DATETIME` does not move. This frontend converts explicit
text and prepared `INSERT ... VALUES` parameters from the fixed session offset
to UTC and converts direct text and binary result columns back. A non-UTC
`SELECT` over a table containing TIMESTAMP is supported only for direct columns
from one unfiltered base table: joins, filters, ordering, expressions and wider
shapes are refused because the engine would evaluate the UTC text as local
time. Non-UTC `UPDATE` and `DELETE` on such a table, `INSERT ... SELECT`,
implicit column lists and AUTO_INCREMENT inserts with TIMESTAMP are also
refused. A prepared statement must be prepared again after the offset changes.
Non-UTC session-local clock calls are refused, as are INSERTs into a table with
`DATETIME DEFAULT CURRENT_TIMESTAMP` and UPDATEs on a table with
`DATETIME ON UPDATE CURRENT_TIMESTAMP`; the engine's UTC clock cannot supply
the required local wall time there.

MySQL's range — `1970-01-01 00:00:01` through `2038-01-19 03:14:07`, both
boundaries measured — is enforced on writes. What remains unsupported is the implicit
`DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP` MySQL gives the first
`TIMESTAMP` column under `explicit_defaults_for_timestamp=OFF`; a written
`DEFAULT CURRENT_TIMESTAMP` at whole-second precision is accepted. The input surface and the calendar check
are a `DATETIME`'s, so `'2026-02-30 00:00:00'` answers 1292 here as it does
there.

What a client reads about a `TIMESTAMP` column does match. `SHOW CREATE TABLE`
and `SHOW COLUMNS` print `timestamp`, and a nullable one prints `timestamp NULL
DEFAULT NULL` where a nullable `DATETIME` prints only `datetime DEFAULT NULL` —
measured, and the one place the two types are spelled differently. A result
column reports type 7 with length 19 plus any fractional point and digits,
decimals equal to its declared fsp, and the binary flag.

Every column type this frontend answers crosses the binary protocol as well as
the text one. `CHAR`, `DECIMAL`, `DATETIME` and `TIMESTAMP` each arrived with a
text answer and no binary one, so a prepared `SELECT` of any of them failed
where the same statement over the text protocol worked. MySQL sends a `CHAR` and
a `DECIMAL` as length-encoded text and a temporal value as fields — a length
byte and then that many bytes, nothing at all for a zero value, the date alone
when the time is midnight, and the date and time otherwise — and that is what
these send now. An eleven-byte `DATETIME` or `TIMESTAMP` result carries
microseconds when its declared precision is nonzero; `TIME` uses its own
12-byte signed span form with microseconds.

`FLOAT` is taken, with binary32 rounding before the value is stored. MySQL keeps
a `FLOAT` in binary32; the engine's binary64 slot holds exactly the rounded
binary32 value. Both protocols also round it on the way out: the text protocol
renders the binary32 nearest the stored value, so `0.1` reads back as `0.1`
rather than as the binary64 nearest a binary32 `0.1`, and the binary protocol
sends the four bytes a `FLOAT` column's four bytes are. A later `SUM` now sees
the same rounded inputs as MySQL.

The metadata is measured: `SHOW CREATE TABLE` and `SHOW COLUMNS` print `float`,
and a result column reports `MYSQL_TYPE_FLOAT` with length 12, where a `DOUBLE`
reports 22, both with the not-fixed decimals value. `FLOAT(p)` and `FLOAT(p,s)`
are refused, because `FLOAT(p)` means `DOUBLE` above 24 and `FLOAT(p,s)` is a
display width whose rounding has not been measured. An inline `UNIQUE` is
taken.

`SELECT DATABASE()` is answered from the session, with or without a selected
database. MySQL answers it either way, returning NULL when nothing is selected,
and that is how a client's `USE` reaches the server at all: `com_use` asks
`SELECT DATABASE()` first, so a server that demands a selected database here can
never be given one. `SCHEMA()` is taken as MySQL's synonym, an alias renames the
column, and the column is otherwise named after the call as the client wrote it,
spacing and case included, which is what MySQL 8.4.11 does. The column is a
`VAR_STRING` of length 256 with no flags and `decimals` 31, measured.

`DATABASE()` is also read beside the session's variables and the other calls
answering what a session knows about itself — `select DATABASE(), USER() limit
1`, which the `mysql` client's `status` sends, and Django's `SELECT VERSION(),
@@character_set_client, DATABASE()`, sent on every connection. Measured on
8.4.11 and matched: `USER()`, `SESSION_USER()` and `SYSTEM_USER()` are the name
the client logged in with at the host it came from, and `CURRENT_USER()` —
`CURRENT_USER` without parentheses too — the account the login matched, each a
nullable `VAR_STRING` of 1152 with 31 decimals. Every account here is one for
any host, so `CURRENT_USER()` reads `name@%`. This server looks no name up, so
the host is the client's address, as MySQL writes it under `skip_name_resolve`
(`app@172.17.0.9` on the oracle), with an IPv4 client a dual-stack socket
reports inside IPv6 written as the IPv4 address; a client on the Unix socket
comes from `localhost`. `CONNECTION_ID()` is the ID the handshake sent, a NOT
NULL unsigned `LONGLONG` of 21.

`ROW_COUNT()` and `FOUND_ROWS()`, each a NOT NULL signed `LONGLONG` of 21, read
what the last command did, by rules measured one command at a time on 8.4.11: a
fresh connection reads 0 for both; a statement answering rows makes
`ROW_COUNT()` -1 and `FOUND_ROWS()` the rows it answered — a `SHOW` too, but
`SHOW WARNINGS` leaves `FOUND_ROWS()` alone; one answering OK makes
`ROW_COUNT()` the rows its OK reports and leaves `FOUND_ROWS()` alone; one that
fails makes `ROW_COUNT()` -1; and `COM_PING` and `COM_INIT_DB` make
`ROW_COUNT()` 0. Where the effect was not measured, or is one this server does
not report, the count is not known and a call reading it is refused until a
statement says what it is again: `FOUND_ROWS()` after an `UPDATE` (MySQL makes
it the rows the `UPDATE` matched), after `SHOW ERRORS`, and both after a
prepared-statement command, a `COM_RESET_CONNECTION`, or a `COM_QUERY` refused
before any statement of it ran. `SQL_CALC_FOUND_ROWS` on a statement's own
`SELECT` asks `FOUND_ROWS()` for the rows the statement answers without its
`LIMIT` instead — measured on 8.4.11, its groups or its distinct rows, and all
of them past an offset beyond the last — and warns 1287, as MySQL, which has
deprecated it, does. The count is worked out inside the statement's own read
of the database: its `LIMIT` is written as a call counting the statement
without its `ORDER BY` and `LIMIT`, which the engine works out once before the
first row, so the count and the rows answered come from the same rows. In a
subquery or a `UNION` it is refused, where MySQL answers 1234 for the first; so
is one beside a bound value or a `WITH`, which the count would read a second
time, and one in a prepared statement, whose count would not be read back.
MySQL also warns 1287 for `FOUND_ROWS()` itself, which this server does not. A
connection the runtime did not accept knows neither its login nor its ID nor
its counts, and refuses each of these calls.

`SHOW [FULL] PROCESSLIST` lists the sessions logged in to this server under
the asking account, which is what MySQL 8.4.11 lists for an account without
`PROCESS` — every account here is one — measured: every connection of the
account, a pool's others among them, in the order of their IDs and none of
another account's. Each row carries the connection ID, the account, the host
as `SHOW PROCESSLIST` writes it — the client's address and port over TCP,
`localhost` on the socket — the selected database, `Sleep` with an empty state
and no statement for a session waiting, or `Query` (`Execute` for a prepared
statement) with the statement for one running, and `Time`, the whole seconds
since the session last started or finished a command; the asking session's own
row reads `init`, and without `FULL` a statement is cut to 100 characters, each
as measured, in MySQL's columns. Two things differ: another session running a
statement reads `executing`, where MySQL names the step it is at (`User sleep`,
`Sending data`), and a prepared statement shows its `?` where MySQL writes the
values bound to it. A session the runtime did not accept is not listed and
cannot list them.

`SHOW [GLOBAL | SESSION] STATUS [LIKE 'pattern']` answers the three counters
this server keeps, and no row for any other name, the way `SHOW VARIABLES`
does: `Threads_connected`, `Uptime` and `Uptime_since_flush_status`, in name
order and in `SHOW VARIABLES`' columns read from `session_status` or
`global_status`, the server's value in either scope, measured. `Uptime` counts
from when this server opened its databases, and `Uptime_since_flush_status`
with it, `FLUSH STATUS` being refused. `Threads_connected` counts the sessions
logged in, where MySQL also counts a connection still in its handshake.

`SELECT variable_value [AS alias] FROM performance_schema.session_status WHERE
variable_name = 'name'` — Laravel's `db:show` counts the connections so — and
the same read of `global_status` answer one of those three counters, over text
and prepared: measured on 8.4.11, the name is matched without regard to case,
and the one column is a nullable `VAR_STRING` of 4096 named after the alias,
whose origin is the table's `VARIABLE_VALUE`. Any other counter, and any other
read of those tables, is refused rather than answered with no row, which MySQL
answers only for a name it has not got.

These calls are read only as a whole `SELECT` of such calls and variables:
beside a `FROM` or inside an expression they are refused. Prepared, such a
`SELECT` is answered as it is over text. Every
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

A table that is not there answers 1146 and a name that is no column answers
1054, in MySQL's own words, where this server used to refuse the statement with
1235 before it looked the name up. Measured on MySQL 8.4.11, which opens every
table a statement names before anything else: `SELECT`, `INSERT`, `UPDATE`,
`DELETE`, `DESCRIBE`, `SHOW COLUMNS`, `SHOW INDEX`, `SHOW CREATE TABLE`, a join
and a subquery naming a table that is not there all answer 1146, SQLSTATE
`42S02`, `Table 'probe.missing' doesn't exist`, the first such table in the
order written and, under `lower_case_table_names=1`, which this server reports,
with both names lowercased; a `WITH` name is a table in the statement after it
and, under `WITH RECURSIVE`, in its own body too. A name that is no column
answers 1054, SQLSTATE `42S22`, `Unknown column 'nope' in 'where clause'`, the
name as written and qualified as written — `p.id` when `p` names no table —
checked in MySQL's order: a `SELECT`'s select list (`field list`) before its
`WHERE`, an `UPDATE`'s `WHERE` before its `SET`, which is the `field list`, a
`DELETE`'s `WHERE`, an `INSERT`'s column list and `VALUES`. Frameworks tell
whether a table exists by catching 1146 — Laravel, Django and Rails all do.
This server answers the same, text and prepared alike, whenever it would
otherwise refuse such a statement, by reading the statement again for the
tables it names; a temporary table and a view are tables it names. Only a
session that may query the whole database is told a table is not there, the
way MySQL answers 1142 first to one that may not. For 1054 the statement must
read or write one table and use only the expressions this follows — columns,
written values, operators, `IN`, `BETWEEN`, `LIKE` and calls over them; any
other shape keeps this server's own answer rather than risk naming the wrong
clause. These two messages carry the name, as MySQL's do; every other message
here stays the short fixed one.

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

The character set a client names in its handshake no longer shuts it out.
MySQL's own interactive client sends the one from the shell's locale, and a
shell with no UTF-8 locale sends latin1; `mysqldump` sends utf8mb4 whatever
the locale. Measured on MySQL 8.4.11, a server takes any collation ID in the
SSLRequest and the handshake response and sets `character_set_client`,
`character_set_connection`, `character_set_results` and
`collation_connection` from it: 8 reads back latin1 and `latin1_swedish_ci`,
224 `utf8mb4_unicode_ci`; an ID it has no collation for — 0, 17, 254 — gives
its default; and ucs2, utf16, utf16le and utf32 are answered 1231, SQLSTATE
`42000`, `Variable 'character_set_client' can't be set to the value of
'ucs2'`, in place of the final OK. `COM_RESET_CONNECTION` then puts the
session back on the server's default rather than the handshake's. This server
does the same for utf8mb4's collations, as described under `SET NAMES`, and
for `latin1_swedish_ci`, and answers 1231 for those four, with a message that
does not name the character set. latin1 is kept as `SET NAMES latin1` would
leave it, with the refusals that come with naming latin1: a statement outside
ASCII and a result set holding anything outside ASCII are refused until the
session names utf8mb4, which the `SET NAMES utf8mb4` a driver sends does. Any
other character set —
utf8mb3, binary, another latin1 collation — is answered 1235, `This server
does not take the character set the client asked for; connect with utf8mb4`,
in place of the final OK once the credentials are checked. Before, the server
closed the socket at the SSLRequest with no answer and the client reported
`SSL connection error: unexpected eof`. The two captured handshakes, utf8mb4
and latin1, are pinned as tests and both decode.

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
| `CREATE TABLE` | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend tests`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [key clause](conformance/cases/p0/create-table-key-clause.json), [table options](conformance/cases/p0/create-table-options.json), [unique key](conformance/cases/p0/create-table-unique-key.json), [composite key](conformance/cases/p0/create-composite-key.json) oracle cases, [P0 manifest](conformance/Makefile), [`mysql_async` Unix E2E](runtime/tests/unix_e2e.rs) | Conservative marked-DDL subset only, including ordinary signed `INT`/`INTEGER PRIMARY KEY`, a key over a word — `VARCHAR(n)` or `CHAR(n)`, which is what every migration tool keeps its own record in — and identity-backed v3 `AUTO_INCREMENT` DDL. Legacy v2 identities remain readable when their envelope is rewritten. A key column reads back `NOT NULL` whether or not the statement said so, which is what MySQL prints; a `TEXT` key is refused, MySQL wanting a length there (1170), and a word cannot be counted (1063). Uniqueness over a word folds case, the way every other reading of a word here does: measured on 8.4.11, `'ALPHA'` after `'alpha'` is 1062 both there and here. Fresh v3 text keys fold accents and case under UCA9. The key may be written on the column or as a `PRIMARY KEY (col)` clause of its own, the spelling MySQL prints and every dumped schema carries, and an inline `KEY` or `UNIQUE` key becomes one `CREATE INDEX` or `CREATE UNIQUE INDEX` under the same all-or-nothing transaction; a single-column clause is moved onto the named column, while a clause over several columns is retained as a composite key. Every key column without an explicit nullability clause becomes `NOT NULL`, as MySQL prints it; explicit `NULL` and `DEFAULT NULL` key columns, `USING BTREE`, `DESC`, a missing column, and two primary keys remain refused. Ordinary primary keys lower to a regular SQLite `INT NOT NULL PRIMARY KEY` without a rowid alias; the durable v3 marker retains source integer spelling and the `ENGINE = InnoDB` label. A table may carry the trailer MySQL prints after every table — `ENGINE=InnoDB`, a `CHARSET`/`CHARACTER SET` of `utf8mb4` and a `COLLATE` of `utf8mb4_0900_ai_ci`, in any of MySQL's spellings — which names the table this makes anyway and is taken and left out, so a printed schema can be handed straight back; another character set, another collation, another engine, `ROW_FORMAT` and a repeated option are all refused. A table `COMMENT` is kept at the end of the stored MySQL DDL, after the collation, and carried across every rewrite of the table: measured on 8.4.11, `SHOW CREATE TABLE` prints it last as ` COMMENT='...'`, quoted the way a column comment is, and not at all when it is empty, and `information_schema.TABLES.TABLE_COMMENT` and the `Comment` of `SHOW TABLE STATUS` report its text. `AUTO_INCREMENT=<n>` says where a counted table's numbering starts, which mysqldump writes on every table that has held a row: the table is made and the allocator's mark is then raised so the first row takes that number. A column may carry `ON UPDATE CURRENT_TIMESTAMP` and a `COMMENT`, both of which live in the stored MySQL DDL alone and are taken off before the engine's parser sees the rest. An ordinary-PK table is rewritten through the same renderer every other table's is, which is what lets an `ALTER TABLE` run against one; a counted table goes through that renderer told which column it counts on and what that column was declared as, so its rewrite keeps the declared type and the marker that makes it counted — the engine holds the column as a rowid alias whatever it was declared as, so a rewrite that read the type back off the engine turned a `BIGINT` key into an `int` one and narrowed how high the table could count with it; a rewrite drops the source integer spelling and the `ENGINE` label, neither of which a client can see. Auto-increment tables remain creatable, reopenable, and replayable through the identity-backed embedded frontend, with execute-only literal `INSERT ... VALUES` generation in registry-selected embedded sessions. The authorized command adapter executes the checked DDL subset through text `COM_QUERY` after database selection and authorization; the external-driver E2E covers `CREATE TABLE`. Qualified names and wider forms remain rejected. `TEMPORARY` is taken for an ordinary table and gated only for the AUTO_INCREMENT form. `IF NOT EXISTS` is taken for every form, looking the name up first so a table already there is left exactly as it stands and warned about with note 1050; it is not printed back, MySQL not printing it either. That same lookup answers 1050 as an error where the statement did not say the words, which is what MySQL answers; a `TEMPORARY` table is left out of it, one being allowed to shadow a permanent table of the same name. Non-binary character contexts and prepared DDL remain closed. `AS SELECT` is taken over a checked one-table `SELECT` whose projected items are plain columns, aliased or not, or a lone `*`: the new columns are read out of the source table's stored DDL, keeping type, `NOT NULL` and `DEFAULT`, dropping keys, and replacing a dropped `AUTO_INCREMENT` with a zero default, and the `CREATE` and its `INSERT` run inside one transaction. It reports the rows copied. Expression columns, string defaults, declared columns beside the `SELECT`, `IF NOT EXISTS` and `TEMPORARY` are rejected there. `LIKE` — `CREATE TABLE new LIKE old`, or `(LIKE old)` — makes the new table from what `SHOW CREATE TABLE` prints for the old one, so it takes the same checked path a printed schema handed back does: measured on 8.4.11, the new table takes the source's columns, keys, collation and comment and none of its rows, a foreign key's own index stays as a plain key while the constraint is left behind, and the counter starts again from 1. MySQL looks at the source first — 1146 when it is not there and 1347 when it is a view, even under `IF NOT EXISTS` with a new name that is taken — then answers 1066 for the same name twice and 1050 (a note under `IF NOT EXISTS`) for a new name that is taken, and this answers the same in the same order. Like any DDL it commits what came before it. A source with a `CHECK`, which `SHOW CREATE TABLE` cannot print yet, a `TEMPORARY` copy and either name written with its database are refused. |
| `ALTER TABLE` | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/alter-counted-table.json), [P0 manifest](conformance/Makefile), [architecture limits](../docs/mysql-compatibility-mode.md) | Checked text DDL dispatch accepts several supported operations in one statement. View- and trigger-dependent rewrites retain the documented restrictions. An ordinary-PK table takes the column operations, and a `MODIFY`/`CHANGE` of the key column itself is refused. A table that counts its own ids takes them too and goes on counting, its rewrite writing the counted column the way it was declared; `DROP COLUMN`, `RENAME COLUMN` and `MODIFY COLUMN` of that column are refused, where MySQL drops it and leaves an ordinary table. Prepared DDL remains unsupported. Several operations in one statement are split into one MySQL statement each and run inside one transaction, so the statement applies whole or not at all. `ADD INDEX`, `ADD KEY`, `ADD UNIQUE INDEX` and `DROP INDEX` become one `CREATE INDEX` or `DROP INDEX` each, under the same all-or-nothing transaction, and answer 1061 for a name the table already carries and 1091 for one it does not. `DROP KEY` is taken as MySQL's other spelling of `DROP INDEX`. The name `PRIMARY` is reserved for the primary key. Supported index and column operations may be mixed in one statement; the final foreign-key child-index coverage is checked before commit, so an index can be replaced atomically. `MODIFY COLUMN` and `CHANGE COLUMN` restate one column whole — an attribute the statement does not restate is dropped, as MySQL drops it — and become the engine's `ALTER COLUMN`; `ADD COLUMN` takes `FIRST` and `AFTER x`: the table is written again with the column standing there and its rows carried across, its indexes, child foreign keys and counter as they were. Rewriting a referenced parent table remains refused. `AFTER` the last column is the place the column takes anyway and runs as the ordinary statement; `AFTER` a column the table has not got answers 1054. `MODIFY` and `CHANGE` take a place too, the column leaving the list before the place is counted and its values coming across under the name it ends up with; a `CHANGE` onto a name the table already carries answers 1060, and moving the counted column or the key column is refused. An unknown column answers 1054. `RENAME INDEX a TO b` and `RENAME KEY` — Laravel's `renameIndex`, Rails' `rename_index` — write the index again under its new name, and every index after it again too, so it keeps its place among the table's keys as MySQL keeps it; measured on 8.4.11, the renames of one statement are read against the names the table had before it, so two names can be swapped, a name the table has not got is 1176 and a new name already taken is 1061. An index the engine made for a foreign key comes back as an ordinary one, which MySQL keeps where it would have dropped the unrenamed one for a later index covering the same columns. `ALTER COLUMN c SET DEFAULT v` and `DROP DEFAULT` — Rails' `change_column_default` — are written as a `MODIFY COLUMN` restating the column from its stored DDL with its new default, so the default is checked and printed the way one written in a `CREATE TABLE` is; `DROP DEFAULT` on a column that may hold NULL is refused, MySQL then printing the column with no `DEFAULT` at all. A `MODIFY` or `CHANGE` that keeps a `DECIMAL` column's size and sign is taken and the column reads the same at once; one that changes them, or turns a column into or out of a `DECIMAL`, is refused, MySQL writing every stored value again in the new form — measured on 8.4.11, a 1.5 in a `DECIMAL(8,2)` reads `1.500` after `MODIFY d DECIMAL(12,3)` — where the engine keeps the text each was written as. `COMMENT = 'x'` — Laravel's `alter table t comment = 'x'` and Rails' `ALTER TABLE t COMMENT 'x'` — changes the table's comment; the engine is asked to rename the table's first column to its own name, the one change that alters nothing, and the table's rewritten DDL carries the new comment. `ENGINE=InnoDB` — which Django and Rails write in some migrations — names the engine every table here has: measured on 8.4.11, MySQL rebuilds the table and nothing a client can see changes, the counter included, so it commits what came before, as DDL does, and changes nothing; another engine is refused, MySQL taking it and printing it back. `AUTO_INCREMENT = n` sets where a counted table's numbering goes on from: measured on 8.4.11, the next row takes `n` or one past the highest id the table holds, whichever is larger, and a table counting nothing takes the statement and changes nothing. MySQL moves the counter back that way once the rows above it are deleted, and the allocator only moves forward, so a change that would move it back is refused; so is a number past the column's type, which MySQL takes and answers 1467 for at the next row. Each of these five is taken only on its own in a statement, and a view is refused where MySQL answers 1347. `ADD [CONSTRAINT name] CHECK (expr)` — Rails' `add_check_constraint`, Django's `CheckConstraint` — and `DROP CHECK name` or `DROP CONSTRAINT name` write the table again with the constraint added or taken away and its rows carried across, so a row already there that breaks a new constraint answers 3819 and leaves the table as it was, as MySQL does; every constraint is then stored under its name, so dropping one leaves the others theirs. Measured on 8.4.11: an unnamed one added takes one past the highest number the table's names carry, a name is matched without regard to case, a name the table has not got is 3821 and one any table's constraint has is 3822, and one naming a counted column is 3818, which is refused here. A row breaking a `CHECK` answers 3819, where it used to answer 1062. A table that counts its own ids takes a table-level `CHECK` too. |
| `DROP TABLE` | partial | partial | experimental | planned | partial | [`checked parser`](parser/drop_table.rs), [`frontend session`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Accepts one or more non-internal table names with optional `IF EXISTS` and one trailing semicolon, which is how Laravel's `migrate:fresh` drops every table at once. A name may be qualified by the database the session is in — Laravel 12 writes ``drop table `laravel`.`cache`, `laravel`.`jobs`, ...`` — named without regard to case under the `lower_case_table_names=1` this server reports; one qualified by another database is refused, where MySQL drops that database's table. Measured on MySQL 8.4.11 and matched: a statement naming a table that is not there drops none of the others and answers 1051; `IF EXISTS` drops the rest and leaves one note for each missing table; a table named twice is 1066; and a parent goes with its child whatever order they are named in, so a child is dropped first. With foreign key checks on, a table another table's foreign key names answers 3730 and the statement drops nothing, unless it drops that other table too; a table whose key names itself is dropped; with the checks off it goes — each measured. A trailing `RESTRICT` or `CASCADE`, which GORM writes, is taken and changes nothing, as in MySQL, where `CASCADE` does not reach the child either; both together are 1064. Other clauses are rejected. A failure dropping one table after another has gone leaves the earlier one dropped, where MySQL's atomic DDL keeps it. Base-table removal, missing table/view handling, `sql_notes` warnings, and the preceding-transaction commit boundary are covered. |
| `TRUNCATE TABLE` | partial | partial | experimental | planned | partial | [`checked parser`](parser/truncate_table.rs), [`frontend session`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Accepts exactly one unqualified non-internal table name, with the `TABLE` keyword optional and one trailing semicolon. Qualified or multiple names, extra clauses, and comments are rejected; prepared DDL remains unsupported. An unfiltered `DELETE` does the emptying between a commit on each side, so the statement cannot be rolled back and the write before it is committed, and it reports 0 affected rows. An unknown name and a view both answer 1146. A table that counts its own ids is taken and its counter starts again: the table is written again from what it was stored as, taking a fresh allocator identity, and its indexes are written again beside it. A table carrying a trigger is refused there, a trigger not being the table's own row. A table another table's foreign key names answers 1701, which is what MySQL answers. |
| Indexes | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Existing checked text DDL dispatch accepts conservative ordinary and unique index creation. Prepared DDL remains unsupported. |
| Views | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Existing checked text DDL dispatch accepts simple one-table view creation. `DROP VIEW [IF EXISTS] a, b [RESTRICT | CASCADE]` drops the views it names; measured on 8.4.11 and matched, a name given twice is 1066, a table among the names is 1347 and otherwise a missing name 1051, each dropping none of the others, and `IF EXISTS` drops every view named and notes each other name in the order given — `Note 1347 'probe.plain' is not VIEW` for a table, `Note 1051 Unknown table 'probe.nope'` for nothing. `RESTRICT` and `CASCADE` change nothing, as in MySQL. A name qualified by its database is refused. `CREATE OR REPLACE VIEW` and `ALTER VIEW` write a view again, dropping the old one and making the new one inside one transaction, so a body the checked `CREATE VIEW` refuses leaves the old view standing; measured on 8.4.11 and matched, either answers 1347 for a table of that name, `ALTER VIEW` answers 1146 for a name nothing has and `CREATE OR REPLACE VIEW` makes the view then, and a plain `CREATE VIEW` over a name a table or a view already has is 1050. `ALTER VIEW` naming its columns or options is refused. A `SELECT` from a view projecting one table's columns reports each column the way the table does — type, length, collation and flags, a primary key's among them — with the view as its original table and the view or the alias it is read through as its table, which is what MySQL 8.4.11 reports; it used to report no table and a text column as a `BLOB`. A view whose one-table `SELECT` carries a `WHERE` of comparisons — a column against a column or a written whole number, word, `TRUE`, `FALSE` or `NULL`, `IS [NOT] NULL`, joined by `AND` and `OR` — is taken: it is held to every rule the same `SELECT` written on its own is held to, a value of another kind than its column refused, and it is made from and kept in the text MySQL prints for it, which is what `SHOW CREATE VIEW` prints. Measured on 8.4.11 and matched: every column is qualified by its table and spelled the way the table declares it while its name keeps the statement's spelling (`SELECT ID` is `` `posts`.`id` AS `ID` ``), each comparison and each run of `AND` or `OR` stands in parentheses of its own, a run is gathered through the parentheses written around its parts, `!=` is written `<>`, and a quote in a word is written `\'`. A condition whose text would read differently once its columns' types are known — a written day against a moment — is refused, the view being translated without them each time the database is opened. A view grouping its rows — `CREATE VIEW v2 AS SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id`, or one aggregating a whole table — is taken the same way when each column is a grouped column or a named `COUNT(*)`, `COUNT(col)`, `COUNT(DISTINCT col)`, or `MIN`/`MAX` of a whole-number or `VARCHAR` column: measured on 8.4.11 and matched, it is kept and printed as `` select `posts`.`user_id` AS `user_id`,count(0) AS `c` from `posts` group by `posts`.`user_id` ``, and a `SELECT` from it reports each column the way MySQL reads it out of the table it gathers the groups into — a grouped column with its own table as its original table and without its key flags (a primary-key `BIGINT UNSIGNED AUTO_INCREMENT` `id` is `NOT_NULL UNSIGNED`), a count as a NOT NULL `LONGLONG` of 21 without the binary flag a `COUNT` from a table carries, the least or greatest of a column in that column's shape without `NOT NULL` or keys, each aggregate naming the view as its original table. A view joining tables — `JOIN`, `INNER JOIN` and `LEFT [OUTER] JOIN` with an `ON`, over any number of tables, each read under its own name or an alias — and a view reading one table under an alias are taken the same way, and a column written without its table names the one table that declares it: measured on 8.4.11 and matched, `SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id` is kept and printed as `` select `u`.`name` AS `name`,`p`.`title` AS `title` from (`users` `u` join `posts` `p` on((`p`.`user_id` = `u`.`id`))) ``, each further join wrapping the ones before it in parentheses of its own, which is the text a dump restores it from. A `SELECT` reading such a view's rows as they come — its columns, under a condition, cut by a `LIMIT` — reports each column the way its table does, keys included, under the view's name as its original table and without `NOT_NULL` for a column a `LEFT JOIN` can leave missing, over the text and the binary protocol alike, measured; one sorting, grouping, `DISTINCT` or aggregating is refused, MySQL then reporting the joined tables as the original tables, without their keys, whenever its plan sorts through a table of its own — which turns on which table the plan reads first, and so on how many rows each holds. A comma between tables, `USING`, `NATURAL`, `CROSS` and `RIGHT JOIN`, a join grouping its rows and `SELECT *` over a join are refused, MySQL printing the first three in forms not followed here. `SHOW COLUMNS` of a view answers `YES` for a column a `LEFT JOIN` can leave missing, keeps none of its tables' keys and answers `0` as the default of a column its table counts, measured, which it used to answer as `NULL`. A view reading a `DECIMAL` column cannot be read from, as before. `SHOW CREATE VIEW [db.]v` answers MySQL's four columns, 1347 for a table and 1146 for nothing; it prints `DEFINER=`user`@`%`` where MySQL prints the definer's host, and the `utf8mb4_general_ci` this server claims where MySQL prints the connection's collation. Prepared DDL remains unsupported. |
| Triggers | partial | partial | rejected | planned | partial | [`trigger reader`](parser/trigger_definition.rs), [`body check`](frontend/session/trigger_body.rs), [`schema_sql`](frontend/schema_sql.rs) | `AFTER INSERT`, `AFTER UPDATE` and `AFTER DELETE` `FOR EACH ROW`, with a body of one statement or a `BEGIN ... END` list of them, each an `INSERT ... VALUES` of one row naming its columns, an `UPDATE` or a `DELETE` of another table whose `WHERE` compares its columns with `=` joined by `AND`. `BEFORE INSERT` and `BEFORE UPDATE` triggers take `SET NEW.column = value, ...` statements, which change the row before it is written: measured on 8.4.11 and matched, each value reads what the ones before it wrote, the trigger's value stands over the statement's, a `NOT NULL` column with no default of its own that the statement left out or gave NULL is taken from the trigger, and the row is then held to its columns and keys as it stands — a value the trigger makes too long is 1406 and one a `UNIQUE` key already holds 1062 — and counted as changed only where a column comes out other than it was. A value written is `NULL`, a word, a whole number, a column of `NEW` or `OLD` — or, in an `UPDATE`, of the row it changes — `CONCAT` of those, a column with a whole number added or taken away, or a reading of the clock (`NOW()`, `CURDATE()`, `CURTIME()` and their other spellings); a value compared is a whole number or a column. Each is held to the column it meets: a word or a text column's value into a text column, a whole number or an integer column's value into an integer column, a `CONCAT` into a text column, a `+` or `-` over an integer column narrower than `BIGINT`, a moment into a `DATETIME` or `TIMESTAMP`, a day into a `DATE` and a time of day into a `TIME`, each with no fraction of a second; what is written is then held to its column by the same write checks every statement meets, so measured on 8.4.11 and matched, `CONCAT('new ', NEW.name)` past its `VARCHAR` answers 1406 and over a NULL name 1048, and the statement that fired the trigger is taken back with it. Naming `NEW` in a `DELETE` trigger or `OLD` in an `INSERT` one is refused where MySQL answers 1363, and writing the trigger's own table where it answers 1442. MySQL keeps a trigger's body as it was written, and so does this: measured, `SHOW CREATE TRIGGER`, `SHOW TRIGGERS` and a dump print `` CREATE DEFINER=... TRIGGER `t` AFTER INSERT ON `posts` FOR EACH ROW `` and then the body with its case, spacing and line breaks, without the whitespace around it or the `;` ending the statement; `SHOW TRIGGERS` lists a table's `INSERT` triggers before its `UPDATE` and `DELETE` ones. `SHOW CREATE TRIGGER` prints the definer's host as `%`, as `SHOW CREATE VIEW` does, and a trigger's name folded to lower case, as a table's is. A trigger reads the clock under the time zone of the session whose statement fires it, so while any trigger of the database reads it, a write in a session whose time zone is not UTC is refused, the engine's clock reading UTC. |
| MySQL-owned file marker | partial | experimental | n/a | n/a | partial | [`core dialect`](../core/dialect/mod.rs), [`fresh-process tests`](../core/multiprocess_tests.rs) | New MySQL files use and enforce format-v2 marker `0x54520224` (`lower_case_table_names=1`). PostgreSQL v1 remains valid; legacy MySQL v1 and unknown/mismatched policy bits fail closed. Offline legacy migration and policy `0` are not implemented. |
| Logical databases | partial | experimental | experimental | planned | partial | [`database registry`](frontend/database_registry.rs), [`DatabaseCatalog`](frontend/database_catalog.rs), [`Unix capability backend`](frontend/filesystem_backend.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`persistent account store`](server/src/persistent_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs), [`Unix runtime`](runtime/src/main.rs), [`core capability`](../core/database.rs), [D007 plan](../docs/mysql-compatibility-plan.md) | The strict admin parser accepts only `CREATE DATABASE`, `ALTER DATABASE`, `DROP DATABASE`, `SHOW CREATE DATABASE`, `USE`, and `SHOW DATABASES`, each database option naming the collation the database gives its tables; trusted embedded sessions and the authorized `COM_QUERY` adapter execute them through the same typed catalog operations. The registry owns main, WAL, two inode-bound metadata sidecars, and one durable AUTO_INCREMENT allocator sidecar per database. Creation initializes and syncs the allocator identity header before sidecar-first publication; acquire, recovery, and drop verify it through retained descriptors. Real-backend failure and replacement-race tests keep recovery fail closed. Registry-selected embedded sessions retain the allocator and execute the narrow generated-ID INSERT slice. The public Unix catalog shares one root across independent sessions without exposing paths or descriptors. Each session owns at most one selected connection; successful switches release the old lease and failed switches preserve it. Names are canonicalized and authorized before catalog access; denied or unavailable policy returns 1045 without revealing existence, while only authorized missing names return 1049. Create/drop authorization receives the target name, use shares the connect action, list is global and all-or-nothing, and selected-database queries are reauthorized on every command. The same-UID Unix worker supplies the persistent policy and catalog to a real protocol stream; the standalone runtime owns the `RuntimeUnixServer` accept loop and worker reaper. The CI cross-UID external-driver E2E covers `USE`, ordinary writes, prepared writes, and reads through this path. Preopened `VACUUM`, physical restore without re-key/regenerated sidecars, and shared-WAL/MVCC authority remain unsupported; the current protocol surface is the documented conservative DML subset. |
| `SHOW TABLES` | partial | partial | experimental | planned | partial | [`checked parser`](parser/lib.rs), [`table/view listing`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Accepts plain `SHOW TABLES` and confirmed `SHOW FULL TABLES`, each with an optional single semicolon. `SHOW FULL TABLES FROM/IN` may explicitly repeat the selected database, as Connector/J does; another database is refused. A database must already be selected, and the selected database must pass `DatabaseAction::Query` authorization before catalog access. Returns user-visible base tables and views in name order, excluding SQLite/Turso internal tables; when database-wide `Query` is denied, the result is filtered to tables granted through the table `Select` action. The catalog scan uses a 4,097-row sentinel and the protocol result is bounded to 4,096 rows, per-value size, and total retained result memory. A `LIKE 'pattern'` filters the list and puts the pattern in the column name, so `SHOW TABLES LIKE 'alpha%'` answers a column called `Tables_in_probe (alpha%)`; measured on MySQL 8.4.11 the pattern matches a table name by case, unlike every other `SHOW ... LIKE`, which matches whatever the case. Plain `SHOW TABLES FROM/IN` and `WHERE` remain unsupported. |
| `information_schema.TABLES` query | partial | n/a | experimental | n/a | partial | [`catalog tables`](frontend/catalog_tables.rs), [`checked parser`](parser/lib.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-tables.json), [P0 manifest](conformance/Makefile) | A table the engine scans, answered by the ordinary `SELECT` path over eight of MySQL's columns, with `DATABASE()` and `SCHEMA()` read as the selected database after the `FROM`. Selected-database `Query` authorization runs before catalog access; when database-wide `Query` is denied, the result is filtered through table `Select` grants. The result is bounded and lists user tables and views. The checked MySQL oracle case/golden is a reference contract and is listed in the P0 manifest, but it is not a Turso execution gate. Other `information_schema` providers and cross-database coverage remain incomplete. |
| `information_schema.STATISTICS` query | partial | n/a | experimental | n/a | partial | [`catalog tables`](frontend/catalog_tables.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-statistics.json), [P0 manifest](conformance/Makefile) | A table the engine scans, so the ordinary `SELECT` path answers it: any projection of MySQL's eighteen columns, a wildcard included, any `WHERE` over them and any `ORDER BY`. One row per column of every index of every table the session may see, the primary key first as `PRIMARY`. `CARDINALITY` is always NULL — MySQL answers a cached estimate of distinct values the engine keeps no equivalent of, and NULL where it has none. Every reported shape is pinned to the MySQL 8.4.11 golden. A call over one of these columns is refused, its shape not having been measured. What a session may see is filtered exactly as for `TABLES`. |
| `information_schema.KEY_COLUMN_USAGE` query | partial | n/a | experimental | n/a | partial | [`catalog tables`](frontend/catalog_tables.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-key-column-usage.json), [P0 manifest](conformance/Makefile) | A table the engine scans, answered by the ordinary `SELECT` path. One row per column of every primary key, unique key and foreign key of every table the session may see; a plain index constrains nothing and has no row. All twelve of MySQL's columns are answered, every shape pinned to the 8.4.11 golden. A foreign key reports its parent table and column and its position in the key it references; a primary or unique key leaves those NULL. A key written without a name is reported as `t_ibfk_N`, matching `SHOW CREATE TABLE`. A wildcard is answered. What a session may see is filtered exactly as for `TABLES`. |
| `information_schema.TABLE_CONSTRAINTS` / `REFERENTIAL_CONSTRAINTS` queries | partial | n/a | experimental | n/a | partial | [`catalog tables`](frontend/catalog_tables.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-table-constraints.json), [P0 manifest](conformance/Makefile) | Tables the engine scans, answered by the ordinary `SELECT` path. `TABLE_CONSTRAINTS` reports one row per primary key, unique key and foreign key of every table the session may see; `REFERENTIAL_CONSTRAINTS` reports one row per foreign key with the key it references, its `MATCH_OPTION`, and the `UPDATE_RULE` and `DELETE_RULE` it was written with — measured, a key written with no rule reads back as `NO ACTION` and `RESTRICT` reads back as written. Both answer all of MySQL's columns, every shape pinned to the 8.4.11 golden. A `CHECK` constraint has a row of type `CHECK` under the name MySQL gives it, and `CHECK_CONSTRAINTS` answers each one's clause as MySQL writes it where that was measured. A wildcard is answered. What a session may see is filtered exactly as for `TABLES`. |
| `information_schema.COLUMNS` contract | partial | n/a | experimental | n/a | experimental | [`checked parser`](parser/lib.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-columns.json), [P0 manifest](conformance/Makefile) | Any projection of `COLUMN_NAME`, `ORDINAL_POSITION`, `COLUMN_DEFAULT`, `IS_NULLABLE`, `DATA_TYPE`, `COLUMN_TYPE`, `COLUMN_KEY`, `EXTRA` and `COLUMN_COMMENT`, a `TABLE_SCHEMA = DATABASE()` or named-database filter, a validated selected-database table/view target, and ordinal ordering — written with or without an explicit `ASC`, which asks for the order these rows come back in anyway — are accepted by the checked parser and provider. `DATA_TYPE` is the kind of a column without its size or sign: measured on 8.4.11 over one of every type, it is `COLUMN_TYPE` up to the first `(` or space, so `varchar(8)` reads `varchar`, `int unsigned` reads `int`, `decimal(8,2)` reads `decimal`, `tinyint(1)` reads `tinyint` and `enum('a','b')` reads `enum`; MySQL reports it as a blob of 201326580 that may be null where `COLUMN_TYPE` is one of 67108860 that may not, and this reports the same. The former fixed `records` target is now arbitrary per query; the pinned `records` case/golden remains the reference contract. The narrow unnamed explicit column-`NULL` form is durable and its frontend metadata is tested, including the restored `MEDIUMINT NULL` fixture. Named or conflicting nullable attributes remain rejected. Selected-database `Query` authorization runs before lookup, with the table `Select` fallback for the requested target; missing or denied targets return an empty result and internal tables remain hidden. Golden metadata is pinned, and scan, row, value, packet-payload, and retained-memory bounds are checked before staging output. The pre-release `MySqlInformationSchemaColumnsQuery` now stores a private target and is no longer `Copy`; callers construct it through the parser and read `table()`. Every other shape — a wildcard, a join, a filter or an ordering over any of MySQL's twenty-two columns, prepared or not — is answered by a table the engine scans, whose rows the session works out from the same metadata before the statement runs; see the prose above. |
| `information_schema.SCHEMATA` / `ROUTINES` queries | partial | n/a | experimental | n/a | partial | [`catalog tables`](frontend/catalog_tables.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [adapter tests](server/src/frontend_adapter/tests/information_schema_wildcards.rs) | Tables the engine scans, each answering all of MySQL's columns, a wildcard included. `SCHEMATA` lists every database the session may list, or the one it is in; `ROUTINES` never holds a row, stored programs being refused. With no database selected, only `SELECT SCHEMA_NAME FROM information_schema.SCHEMATA` is answered. |
| `LIMIT ?` / `LIMIT ? OFFSET ?` / `LIMIT ?, ?` | partial | partial | n/a | n/a | partial | [`limit renderer`](parser/translate.rs), [`row count validator`](frontend/session.rs) | A row count binds like any other parameter. Each spelling is rendered as it was written, so a `?` keeps the ordinal the client bound it at — the comma spelling writes the offset first. What is bound is held to a whole number at or above zero, because the engine reads a negative row count as no limit at all where MySQL refuses one. A `LIMIT` in an `UPDATE` or `DELETE` still takes a written number only. |
| `UPDATE ... SET` assigning arithmetic over the row — `SET n = n + 1` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/update-arithmetic-assignment.json), [P0 manifest](conformance/Makefile) | A column is read in an assignment, and `+`, `-` and `*` over one. Division is refused: measured, `b / 2` over 101 answers 50.5 in MySQL and 50 in the engine. Counting past a column's range is refused and the row keeps what it had, where MySQL answers 1690. A value naming a column the same `SET` has already assigned is refused, because MySQL reads the assigned value there and the engine reads the row as it was. Every answer is pinned to the 8.4.11 golden. |
| `CURDATE()` / `NOW()` / `CURTIME()` as a value to write | partial | partial | n/a | n/a | partial | [`value renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/insert-now-value.json), [P0 manifest](conformance/Makefile) | Written by `INSERT ... VALUES`, `INSERT ... SET`, `ON DUPLICATE KEY UPDATE` and `UPDATE ... SET`. The column puts the value into the form it holds, so a moment into a `DATE` keeps the day and a day into a `DATETIME` becomes midnight, both measured. A moment into a word is the moment written out and one too wide is refused with 1406. Two differences: MySQL records note 1292 for the time dropped going into a `DATE` and this drops it quietly, and a moment into a number is refused here where MySQL runs it together into a fourteen-digit one. Every answer is pinned to the 8.4.11 golden. |
| `COUNT(*) OVER ()` — a window over the whole result | partial | partial | n/a | n/a | partial | [`window reader`](parser/static_select_metadata.rs), [oracle case](conformance/cases/p0/select-window-over-the-whole-set.json), [P0 manifest](conformance/Makefile) | An aggregate over a window naming neither a partition nor an order answers the whole set's value beside every row, in the shape its windowed form already reports. A ranking over the same window keeps its refusal, having no order to rank by. |
| `WHERE <column> = '...' COLLATE ...` | partial | partial | n/a | n/a | partial | [`comparison renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-comparison-collate.json), [P0 manifest](conformance/Makefile) | Written on the column or on the value, either way. `utf8mb4_bin` compares bytes with PAD SPACE and `utf8mb4_0900_ai_ci` compares under frozen UCA9 weights. A collation over a `LIKE`, over a membership test, over a number, over a bound value, or from another character set is refused. |
| `ORDER BY <column> COLLATE ...` | partial | partial | n/a | n/a | partial | [`order renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-order-by-collate.json), [P0 manifest](conformance/Makefile) | `utf8mb4_bin` uses byte order with PAD SPACE; `utf8mb4_0900_ai_ci` uses frozen UCA9 weights, the same as naming none on a new text column. A collation from another character set is 1253 there and refused here, as is one over something that is not a column. |
| `BIN`, `OCT`, `FIELD`, `ELT` | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`dialect`](frontend/dialect.rs), [oracle case](conformance/cases/p0/select-radix-and-place.json), [P0 manifest](conformance/Makefile) | The engine has none of the four, so the dialect answers them. `BIN` and `OCT` write a whole number out — a negative one by its bits — and refuse a word, which MySQL reads as the number it names. `FIELD` answers 0 for a word that is not among the choices and for one that is nothing at all. `ELT` answers nothing past the last choice. The choices are written out, being what the answer's width comes from. |
| `PI`, `DEGREES`, `RADIANS` | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`call renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-math-readings.json), [P0 manifest](conformance/Makefile) | `PI()` answers 3.141593 — six places, not the whole number — reporting NOT NULL. Turning an angle round is one multiplication and the two work it out alike. `SIN`, `COS`, `TAN`, `ASIN`, `ACOS`, `ATAN`, `EXP`, `LN`, `LOG`, `LOG2` and `LOG10` are refused: two of them already differ in the last place. |
| `REGEXP` / `RLIKE` | partial | partial | n/a | n/a | partial | [`predicate renderers`](parser/translate.rs), [`dialect`](frontend/dialect.rs), [oracle case](conformance/cases/p0/select-regexp.json), [P0 manifest](conformance/Makefile) | Answered for ASCII text by the dialect, matching without regard to case and with regard to accents. Non-ASCII subjects or patterns fail closed because MySQL ICU full case folding differs from Rust regex. Anchors, character classes, repeats, choices, any-character and the negated form are pinned to the golden. A pattern looking ahead or naming a group again, a pattern that does not close, a bound pattern and a match over a number are refused. |
| Arithmetic touching a `DOUBLE` — `d + 1`, `SUM(d) * 2` | partial | partial | n/a | n/a | partial | [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-double-arithmetic.json), [P0 manifest](conformance/Makefile) | A DOUBLE of length 23 with 31 decimals, whichever side the float was on, whichever operator it was, and whatever the other side was. A float swallows the precision rules rather than taking part in them, and so does an aggregate over one. |
| Arithmetic over an aggregate or a decimal — `SUM(amount) * 2`, `amount + 1` | partial | partial | n/a | n/a | partial | [`arithmetic classifier`](parser/static_select_metadata.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-aggregate-arithmetic.json), [P0 manifest](conformance/Makefile) | An aggregate stands where a column stands. Three measured rules cover adding, multiplying and dividing, over whole numbers and decimals alike. Whether the answer is a decimal is not whether it carries places: `SUM(n) + 1` is one and `COUNT(*) + 1` is not. `GROUP_CONCAT`, a deviation and a windowed aggregate are refused. |
| A column beside an aggregate with no `GROUP BY` | partial | partial | n/a | n/a | partial | [`aggregated projection`](parser/translate.rs), [oracle case](conformance/cases/p0/select-aggregated-projection.json), [P0 manifest](conformance/Makefile) | Refused, where MySQL answers 1140 — a column anywhere in the projection, not only one standing on its own. A literal crosses. A window and a subquery do not aggregate the statement, and a `GROUP BY` gives every column a group. |
| `DATE_FORMAT(NOW(), ...)` / `STR_TO_DATE('...', ...)` — a moment that is not a column | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`call renderer`](parser/translate.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-moment-argument.json), [P0 manifest](conformance/Makefile) | A clock reading and a moment written out as a word stand where a column stands. Both report exactly what the column form reports: the shape comes from the format. `NOW`, `CURRENT_TIMESTAMP`, `CURDATE` and `CURRENT_DATE` are the readings taken; a `STR_TO_DATE` reads text, so it takes a word and not a reading. |
| `TIMESTAMPDIFF(<unit>, a, b)` | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`call renderer`](parser/translate.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-timestampdiff.json), [P0 manifest](conformance/Makefile) | Whole units from the first moment to the second, to the microsecond, with a month counted by the calendar — every unit from MICROSECOND to YEAR. Each moment is a date column, `NOW()` or `CURDATE()`, or a written moment; `DATEDIFF` takes the same. A whole number of length 21, where `DATEDIFF` reports 9. A bound `?`, a word naming no moment and `CURTIME()` are refused. |
| `USE` / `FORCE` / `IGNORE INDEX` | partial | partial | n/a | n/a | partial | [`table source renderer`](parser/translate.rs), [`hint validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-index-hint.json), [P0 manifest](conformance/Makefile) | Dropped: a hint says which key to plan with and nothing about which rows come back. The keys it names are checked against the table, because one naming a key the table has not got is 1176 in MySQL. Both spellings, a `FOR` scope, several keys at once, an alias and either side of a join are covered. A hint on an `UPDATE` or `DELETE` target is still refused. |
| `WEEK`, `DAYNAME`, `MONTHNAME` | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`week numbering`](parser/date_format.rs), [`result metadata`](server/src/frontend_adapter.rs), [adapter tests](server/src/frontend_adapter/tests/date_arithmetic.rs) | Over a date column, a clock reading or a written moment. `WEEK` takes a written mode from 0 through 7, every one measured around six new years. |
| `UTC_DATE()`, `UTC_TIMESTAMP()`, `UTC_TIME()`, `SYSDATE()` | partial | partial | n/a | n/a | partial | [`clock reader`](parser/lib.rs), [adapter tests](server/src/frontend_adapter/tests/date_arithmetic.rs) | Read wherever `CURDATE()`, `NOW()` and `CURTIME()` are, with their measured shapes. Refused in a session of another zone, as `NOW()` is. |
| `QUARTER`, `WEEKDAY`, `DAYOFWEEK`, `DAYOFYEAR`, `DAYOFMONTH`, `LAST_DAY`, `EXTRACT` | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`call renderer`](parser/translate.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-calendar-readings.json), [P0 manifest](conformance/Makefile) | The engine has none of these by name, so each is counted off what it does have. Every value and every reported shape is pinned to the 8.4.11 golden, `EXTRACT(YEAR FROM ...)` included, which reports a whole number of length 5 where `YEAR` reports a YEAR of length 4. |
| `LIKE CONCAT('%', ?, '%')` — a pattern written in pieces | partial | partial | n/a | n/a | partial | [`LIKE renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-like-concat-pattern.json), [P0 manifest](conformance/Makefile) | The pieces spell one pattern, and written ones are joined into it. A bound piece stays a piece and the join is left to the engine. A piece naming a column and a second bound piece are refused. Backslash escapes are read by the dedicated UCA9 LIKE matcher. |
| `WHERE n = (SELECT MAX(n) FROM t)` — a comparison against a subquery | partial | partial | n/a | n/a | partial | [`comparison renderer`](parser/translate.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-scalar-subquery-comparison.json), [P0 manifest](conformance/Makefile) | A `MIN` or `MAX` over one implicit group, held to the same kind rule `IN (SELECT ...)` holds its columns to, and a `COUNT` against a whole number written out. A plain-column projection is refused unless the subquery picks its row by a key: MySQL answers 1242 over many rows where the engine takes the first. An `AVG` of whole numbers against a whole-number column is compared as MySQL's four-place decimal. `SUM` is refused. |
| `WHERE 1 = 1 AND ...` — a comparison naming no column | partial | partial | n/a | n/a | partial | [`predicate renderers`](parser/translate.rs), [oracle case](conformance/cases/p0/select-constant-predicate.json), [P0 manifest](conformance/Makefile) | Two whole numbers compared, and a bare whole number as the predicate, which is the opening a statement built up in pieces uses. It holds in a `SELECT`, an `UPDATE` and a `DELETE`. A word against a word and a number against a word stay refused, MySQL reading those without regard to case and by coercion. |
| `IFNULL(SUM(n), 0)` / `COALESCE(MAX(n), 0)` — an aggregate with a fallback | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-defaulted-aggregate.json), [P0 manifest](conformance/Makefile) | The shape the aggregate answers on its own, plus NOT_NULL, with any whole number widened to a BIGINT and the length left alone. Over no rows the answer is the fallback rather than NULL. The fallback has to be a whole number, the rule the plain-column form already follows. |
| `ON DUPLICATE KEY UPDATE hits = hits + VALUES(hits)` — a counter stepped | partial | partial | n/a | n/a | partial | [`upsert renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/insert-upsert-counter.json), [P0 manifest](conformance/Makefile) | A bare column is the row already there and `VALUES(col)` the one offered, which the engine calls `excluded.col`; arithmetic joins the two. A name put on the offered row — MySQL 8.0.19's replacement for `VALUES()` — names the same thing, and once it is there a bare column is 1052 and refused. |
| `UPDATE ... SET <column> = <call>` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/update-set-call.json), [P0 manifest](conformance/Makefile) | A call or a `CASE` writes a value worked out from the row, rendered the way a projection renders it. A value reading a column the same `SET` has already written is refused: MySQL takes the assignments left to right and the engine reads the row as it stood. A `CONCAT`, `CONCAT_WS`, `LPAD`, `RPAD`, `LEFT` or `RIGHT` over a `BIGINT UNSIGNED` or a `DECIMAL` with no places, written into a column of words, writes the number's digits — GORM's backfill `SET slug = CONCAT('post-', id)`; measured on 8.4.11 and matched, an id of 18446744073709551615 writes `post-18446744073709551615` and a `DECIMAL(10,0)` of -3 writes `-3`. The same call over a `DECIMAL` with places, over arithmetic on such a column, or written into a column of numbers is refused. |
| `UPDATE ... SET` dividing a column — `SET ratio = n / 2` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/update-set-division.json), [P0 manifest](conformance/Makefile) | Decimal division, rounded to the column's own scale on the way in. The divisor has to be a written number that is not zero, and a fraction written into a whole-number column is refused. |
| A `LIKE` pattern's escape — `LIKE 'a\_b'`, `ESCAPE 'x'` | yes | yes | n/a | n/a | yes | [`LIKE renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-like-escape.json), [P0 manifest](conformance/Makefile) | The clause says what MySQL would have taken: a backslash by default, the named character when one is named, and nothing under `NO_BACKSLASH_ESCAPES`. An escape of more than one character is refused. |
| `RENAME TABLE old TO new` | partial | partial | n/a | n/a | partial | [`rename reader`](parser/alter_table_indexes.rs), [oracle case](conformance/cases/p0/rename-table.json), [P0 manifest](conformance/Makefile), [adapter tests](server/src/frontend_adapter/tests/migration_ddl.rs) | Laravel's `Schema::rename` and Rails' `rename_table` write it. Every pair is written into the `ALTER TABLE` shape and they run inside one transaction, pair by pair in the order written, each against the names the ones before it left — measured on 8.4.11, `RENAME TABLE a TO t, b TO a, t TO b` swaps two tables. A table that is not there by its turn answers 1146 and a new name already taken 1050, a table renamed onto its own name among them, and either leaves every table where it was. A counted table goes on counting under its new name, and a table keeps its comment. A name qualified by its database and a view are refused. |
| `DROP INDEX name ON table` | yes | yes | n/a | n/a | yes | [`index reader`](parser/alter_table_indexes.rs), [oracle case](conformance/cases/p0/drop-index-on-table.json), [P0 manifest](conformance/Makefile) | Written into the `ALTER TABLE` shape the reader already answers. An index that is not there is 1091, and the spelling with no table is refused. |
| `information_schema.COLUMNS` naming its database — `TABLE_SCHEMA = 'db'` | yes | yes | n/a | n/a | yes | [`catalogue reader`](parser/information_schema.rs), [oracle case](conformance/cases/p0/information-schema-columns-named.json), [P0 manifest](conformance/Makefile) | Taken beside the `DATABASE()` spelling. The name is read as it was written, and any database but the selected one answers no rows. |
| A `WHERE` testing a column on its own — `WHERE active` | partial | partial | n/a | n/a | partial | [`predicate renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-bare-flag.json), [P0 manifest](conformance/Makefile) | Read as a comparison against zero, as MySQL reads it. A column of words is refused, and so is one tested in an `UPDATE` or a `DELETE`. |
| `IFNULL` / `COALESCE` with a written word — `IFNULL(email, 'none')` | partial | partial | n/a | n/a | partial | [`defaulted classifier`](parser/static_select_metadata.rs), [oracle case](conformance/cases/p0/select-defaulted-word.json), [P0 manifest](conformance/Makefile) | The column's own width whatever the word's is, NOT NULL, and `VAR_STRING` even over a `CHAR`. A `TEXT` column and a word over a column of numbers are refused. |
| A join `ON` naming a value — `ON t.id = u.team_id AND t.name = 'red'` | yes | yes | n/a | n/a | yes | [`join predicate renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-join-on-value.json), [P0 manifest](conformance/Makefile) | Goes through the reader a `WHERE` comparison goes through, so the value is held to the column's own type. Column against column stays equality; an `ON` in an `UPDATE` or `DELETE` takes columns alone. |
| `ORDER BY` over a call — `ORDER BY LOWER(name)` | yes | yes | n/a | n/a | yes | [`ORDER BY renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-order-by-call.json), [P0 manifest](conformance/Makefile) | Any call whose shape is already known, collated the way a text column is. A random number is refused. |
| A derived table — `FROM (SELECT ...) x` | partial | partial | n/a | n/a | partial | [`derived table renderer`](parser/translate/derived.rs), [oracle case](conformance/cases/p0/select-derived-table.json), [P0 manifest](conformance/Makefile) | The body reads one table and projects its columns, aliased or not, `*`, or — when it aggregates — counts, totals, averages, largest and smallest values and days. A body that aggregates reports the shapes of the table MySQL writes it into, and any other body reports the table's own, a day or a moment in words. A body may join a first table and `LEFT JOIN`s, each column named with its table, which is TypeORM's pagination over a relation: a `DISTINCT` over it reports the table MySQL writes the rows into, and any other statement reads it straight through; an `ORDER BY` without `DISTINCT` and a condition in such a body are refused. An expression in a body that does not aggregate, a `DISTINCT` body, an inner join inside the body, a `LATERAL` one and a missing alias are refused, the last being MySQL's own 1248. |
| `DATE_ADD` / `DATE_SUB`, month ends and the week and quarter units | yes | yes | n/a | n/a | yes | [`shift arithmetic`](parser/shift_moment.rs), [oracle case](conformance/cases/p0/select-month-end-shift.json), [P0 manifest](conformance/Makefile) | The shift is worked out by the frontend rather than by the engine, whose month arithmetic overflows a day the target month has not got. A quarter is three months and a week seven days. A count worked out from a row is refused. |
| `CONCAT` over a number — `CONCAT(name, id)` | partial | partial | n/a | n/a | partial | [`spelled characters`](../mysql/server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-concat-numbers.json), [P0 manifest](conformance/Makefile) | A number is laid end to end with the words, spelling as many characters as its type does. Integers, `BOOLEAN`, `YEAR` and the temporal types are taken; a `DECIMAL`, a `FLOAT` and a `DOUBLE` are refused, MySQL spelling those its own way. |
| `HAVING` naming a projection alias — `HAVING c > 1` | yes | yes | n/a | n/a | yes | [`alias resolver`](parser/translate.rs), [oracle case](conformance/cases/p0/select-having-alias.json), [P0 manifest](conformance/Makefile) | A name is the projection's alias before the table's column, measured, and is resolved to what it stands for before the clause is read. Covers an aggregate alias, the grouped column's alias, two at once, no `GROUP BY`, and an aliased column filtering rows. |
| A `CASE` or `IF` whose branches are numbers | partial | partial | n/a | n/a | partial | [`branch classifier`](parser/static_select_metadata.rs), [oracle case](conformance/cases/p0/select-numeric-branches.json), [P0 manifest](conformance/Makefile) | Taken in a projection and in a `SET`. The answer is a `LONGLONG` as wide as its widest branch plus one for the sign, NOT NULL only when every branch is and there is an `ELSE`. A branch carrying a scale, and a word branch beside a number branch, are refused. |
| A `CASE`, `IF`, `IFNULL` or `COALESCE` over columns, and `CASE col WHEN` | partial | partial | partial | partial | partial | [`branch classifier`](parser/static_select_metadata.rs), [`branch renderer`](parser/translate.rs), [`result metadata`](server/src/frontend_adapter.rs), [adapter tests](server/src/frontend_adapter/tests/conditional_expressions.rs) | The kind every branch shares — the widest integer type, a `DECIMAL` with the most digits either side of the point, a `DOUBLE`, or a `VAR_STRING` four bytes a character — measured on 8.4.11 over both protocols. A `CASE` answers each `DECIMAL` branch at its own scale and `IFNULL`/`COALESCE` at the answer's. Needs the one table's column types; unsigned, `TEXT`, `FLOAT` and temporal columns, a word beside a number, a join, and an `UPDATE` writing one of the new forms are refused. |
| `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` over a `CASE` or `IF` | partial | partial | partial | partial | partial | [`aggregate renderer`](parser/translate.rs), [`result metadata`](server/src/frontend_adapter.rs), [adapter tests](server/src/frontend_adapter/tests/conditional_expressions.rs) | `SUM` and `AVG` answer a `NEWDECIMAL` 22 and 4 digits wider than the `CASE` at its scale, or a `DOUBLE`; `MIN` and `MAX` the `CASE`'s own shape over whole numbers and doubles. `MIN`/`MAX` over a `DECIMAL` or words, and ordering by an `AVG` of one, are refused. Grouped, MySQL drops the binary flag from `SUM`, `COUNT`, `MIN` and `MAX`, which this does not. |
| A word naming a number against a column holding numbers — `WHERE id = '1'` | partial | partial | partial | partial | partial | [`word reader`](parser/written_number.rs), [`comparison renderer`](parser/translate.rs), [`bound values`](frontend/session.rs), [adapter tests](server/src/frontend_adapter/tests/written_numbers.rs) | A whole number spelled out (sign and leading zeroes allowed) against a whole-number or `DECIMAL` column, and a decimal against a `DECIMAL`, is read as that exact number in `=`, `<>`, `<`, `IN`, `NOT IN`, `BETWEEN`, a result-column comparison, an `UPDATE` and a `DELETE`; a word bound as a string against a whole-number column is bound as its number; a whole number below 2^53 against a call answering a whole number (`YEAR(col) = '2026'`). Words MySQL reads as doubles, with warning 1292, or by rules not spelled out here (`'1.5'` against an `INT`, `' 30'`, `'3e1'`, `''`, `'30abc'`), words against a `DOUBLE`, and words past an `i64` are refused. |
| An integer column's display width — `INT(11)`, `TINYINT(1)` | yes | yes | n/a | n/a | yes | [`column renderer`](parser/lib.rs), [oracle case](conformance/cases/p0/create-table-display-width.json), [P0 manifest](conformance/Makefile) | Taken and dropped, which is what MySQL 8.4 does with one; the counted column takes one too. `TINYINT(1)` is kept and is the same stored type as `BOOLEAN`, reporting a length of 1 where `TINYINT` reports 4. MySQL's warning 1681 is not raised. |
| `INSERT` writing an `AUTO_INCREMENT` column its own ids | partial | partial | n/a | n/a | partial | [`written ids`](../mysql/frontend/session.rs), [oracle case](conformance/cases/p0/insert-written-auto-increment.json), [P0 manifest](conformance/Makefile) | The counter is raised past the highest id written, so a later counted row never repeats one. Measured and matched: rows out of order, an id below the counter, a negative id, the reported id being the last row's, and `LAST_INSERT_ID()` staying as it stood. A written 0 or NULL asks the counter for the next number, the way leaving the column out does, and a statement whose every row asks that way is numbered from one reserved range and reports the first of it. A statement mixing a row that names its own number with one that asks is refused, MySQL moving the counter row by row there. |
| `INSERT ... VALUES` with `DEFAULT` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/insert-default-value.json), [P0 manifest](conformance/Makefile) | `DEFAULT` and `DEFAULT(col)` naming that same column ask for the column's own default, and are rendered by leaving the column out — measured, MySQL answers the same value, the same NULL and the same 1364 for both. An `AUTO_INCREMENT` column counts on. Every column of a counted table given `DEFAULT` is the row of defaults, which takes the next number like any other row. `DEFAULT` beside `ON DUPLICATE KEY UPDATE` offers the column's default, as MySQL's offered row does. `DEFAULT` in one row and a value in another is refused, and so is `SET n = DEFAULT` on an `UPDATE`. |
| `UPDATE ... SET <column> = (SELECT ...)` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/update-set-subquery.json), [P0 manifest](conformance/Makefile) | A value taken out of another table. The subquery has to answer exactly one row, which an aggregate over one implicit group does and a plain column does not — MySQL answers 1242 for that one. Reading the table being changed is refused, MySQL's 1093. The column written and the column read are held to the same kind, so a `COUNT(*)` and a word into a column of numbers are both turned away. |
| `UPDATE` / `DELETE` naming rows through a subquery | partial | partial | n/a | n/a | partial | [`DML predicate renderer`](parser/translate.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/dml-subquery-predicate.json), [P0 manifest](conformance/Makefile) | `WHERE id IN (SELECT ...)`, `NOT IN` and `EXISTS`, each answering the rows MySQL answers — `NOT IN` over a list holding NULL matches nothing at all. The subquery's table is authorized as a table the statement reads, and its column is held to the same kind rule a `SELECT` holds it to. A subquery reading the table being changed is refused, where MySQL answers 1093. |
| `SELECT a.*` — a wildcard over one source | partial | partial | n/a | n/a | partial | [`projection renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-qualified-wildcard.json), [P0 manifest](conformance/Makefile) | The source's columns in declaration order, each naming its own table. It mixes with a plain column and with a second wildcard, and an alias renames the source for it. A qualifier carrying a schema — `db.t.*` — is refused. |
| `HAVING` with no `GROUP BY` over an unaggregated statement — `SELECT id FROM t HAVING id > 1` | partial | partial | n/a | n/a | partial | [`row-filter reader`](parser/translate.rs), [oracle case](conformance/cases/p0/select-having-without-group-by.json), [P0 manifest](conformance/Makefile) | MySQL filters rows, not groups, so the test is written into the `WHERE`. It may name only a column the projection carries; an unprojected one is 1054 there and stays refused here. |
| `ORDER BY <column> IS NULL` | partial | partial | n/a | n/a | partial | [`order renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-order-by-nulls.json), [P0 manifest](conformance/Makefile) | The idiom for sending the rows holding nothing last. Both answer the test as 0 or 1 and sort by that, so the orders agree — measured with the flag written either way round and in either direction. The test has to be over a column. |
| `(a, b) IN ((1, 'x'), ...)` | partial | partial | n/a | n/a | partial | [`row list renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-row-in.json), [P0 manifest](conformance/Makefile) | Written out as the question it means — each row's columns joined by `AND`, the rows joined by `OR` — so every column is held to its own type, a word is read under the collation, and a row holding NULL is left out of the `NOT IN` as well as the `IN`. A member that is not a row, or one of a different width, is refused. Every answer is pinned to the 8.4.11 golden. |
| A call on the left of a comparison — `WHERE LOWER(email) = 'a'` | partial | partial | n/a | n/a | partial | [`comparison renderer`](parser/translate.rs), [`answer of a call`](parser/static_select_metadata.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-call-comparison.json), [P0 manifest](conformance/Makefile) | The call says what it answers and the value it meets is held to that. A word is compared without regard to case, the way MySQL compares one after the call answers; a number meets a number; a day and a moment are held to the form one is stored in. The calls answering a real number are left out, and a `?` meets none of them. Every answer is pinned to the 8.4.11 golden. |
| `CAST(col AS CHAR / SIGNED / DATE / DATETIME)` | partial | partial | n/a | n/a | partial | [`cast classifier`](parser/static_select_metadata.rs), [`cast renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-cast.json), [P0 manifest](conformance/Makefile) | The four targets the engine answers exactly what MySQL answers. `CHAR` writes a whole-number or temporal column out, as wide as the column's display width in utf8mb4 bytes. `SIGNED` rounds away from zero before it casts, because MySQL rounds and the engine's cast cuts. `DATE` and `DATETIME` read the day and the moment out. `UNSIGNED`, `DECIMAL`, `CHAR(n)`, a real or `DECIMAL` column written out, and a word read as a number or a day are each refused for a measured reason. `CONVERT(col, <type>)` is read as the same thing; `CONVERT(col USING <charset>)` and the T-SQL spellings are refused. |
| `x + INTERVAL n unit`, `x - INTERVAL n unit` | partial | partial | n/a | n/a | partial | [`operator reader`](parser/static_select_metadata.rs), [`shift renderer`](parser/translate.rs), [adapter tests](server/src/frontend_adapter/tests/date_arithmetic.rs) | Read as the `DATE_ADD` or `DATE_SUB` it is, in a projection, on the right of a comparison and as a value to write, named after the whole operator form. Two shifts in a row are refused. |
| `DATE_ADD` / `DATE_SUB` over a written moment | partial | partial | n/a | n/a | partial | [`call metadata`](parser/static_select_metadata.rs), [`result metadata`](server/src/frontend_adapter.rs) | Measured: a `STRING` of 116 whatever the interval, a day written alone staying a day. Only the `YYYY-MM-DD hh:mm:ss.ffffff` spelling is taken, and a word naming no moment is refused. |
| `DATE_ADD` / `DATE_SUB` over a reading of the moment | partial | partial | n/a | n/a | partial | [`shift renderer`](parser/translate.rs), [`call metadata`](parser/static_select_metadata.rs), [oracle case](conformance/cases/p0/select-shifted-moment.json), [P0 manifest](conformance/Makefile) | Read in a projection, as a value to write, and on the right of a comparison. Measured: shifting `NOW()` answers a nullable `DATETIME` of 19 whatever the interval, and shifting `CURDATE()` a nullable `DATE` of 10 for whole days, months or years and a `DATETIME` otherwise. A shifted day meets a `DATE` column and a shifted moment a `DATETIME` or `TIMESTAMP`. `CURTIME()` is not shifted: a span is not a moment. Which rows each comparison finds is pinned to the 8.4.11 golden. |
| `WHERE` comparison against `CURDATE()` / `NOW()` / `CURTIME()` | partial | partial | n/a | n/a | partial | [`comparison reader`](parser/lib.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-now-comparison.json), [P0 manifest](conformance/Makefile) | Each is rendered as the engine call answering the same value in the same form — `date('now')`, `datetime('now')`, `time('now')` — and meets the column whose form it answers in: a day meets a `DATE`, a moment a `DATETIME` or `TIMESTAMP`, and a time of day a `TIME`, for sameness only. Both spellings of each, with and without parentheses, are read. Any other call on the right of a comparison is still refused. Every answer is pinned to the 8.4.11 golden. |
| `WHERE` comparison against a number written with a fraction — `money > 9.99` | partial | partial | n/a | n/a | partial | [`comparison reader`](parser/translate.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-decimal-literal-comparison.json), [P0 manifest](conformance/Makefile) | Read as the number it names and carried into the rendered SQL as it was written, so the engine reads the same number. It meets any column that holds a number, whole or not; a text column is refused, the mirror of a string against an integer column. A direct comparison against a known exact `DECIMAL` column accepts whole numbers beyond `i64` up to 65 written digits; `IN` with those literals remains refused. A `HAVING` still takes only a whole number, being counted against a count. Every answer is pinned to the 8.4.11 golden. |
| `LOCK TABLES` / `UNLOCK TABLES` | partial | partial | n/a | n/a | partial | [`lock parser`](parser/lock_tables.rs), [`write lock`](frontend/session.rs) | The lock is really held, until `UNLOCK TABLES`: it is the engine's write lock, held by the write transaction the statement opens, and a session that writes while it is held waits and answers 1205. One lock over the whole database rather than one for each table, so `READ` and `WRITE` take the same one and the names are read and let go. The statements between commit together at the unlock, so `START TRANSACTION`, `COMMIT` and `ROLLBACK` are refused while it is held rather than dropping the lock. `READ LOCAL`, `LOW_PRIORITY WRITE` and `LOCK INSTANCE FOR BACKUP` are refused. |
| `SELECT ... FOR UPDATE` / `FOR SHARE` | partial | partial | n/a | n/a | partial | [`lock reader`](parser/translate.rs), [`write lock`](frontend/session.rs) | The lock is really held: the statement takes the engine's write lock by writing no row, and another session that writes while it is held waits for it and answers 1205 once the wait runs out, which starts at MySQL's fifty seconds and is changed by `SET SESSION innodb_lock_wait_timeout`. It is one lock over the whole database rather than one for each row, so it is stronger than MySQL's. Outside a transaction none is taken, which is what MySQL's amounts to there. `SKIP LOCKED` takes the same lock, waiting for it where MySQL would skip the rows another session holds. `NOWAIT` and `OF <table>` are refused. `LOCK IN SHARE MODE` is read as `FOR SHARE`. |
| `WHERE` comparison against a `DATE` / `DATETIME` / `TIMESTAMP` / `TIME` / `YEAR` / `DECIMAL` / `DOUBLE` / `FLOAT` / `ENUM` / `SET` column | partial | partial | n/a | n/a | partial | [`comparison validator`](frontend/session.rs), [`temporal values`](parser/temporal_value.rs), [oracle case](conformance/cases/p0/select-temporal-comparison.json), [P0 manifest](conformance/Makefile) | These columns hold the canonical form MySQL stores, so a comparison against a value already written that way answers the rows MySQL answers, whatever each row was written as. A day and a moment read in order read in time order, so every operator works; a `TIME` runs past a day and carries a sign, so only `=`, `!=`, `<=>` and `IN` are answered for one. A `YEAR` and a real are compared as numbers. A value written any other way is refused rather than rewritten — measured, `d = '2024-1-1'`, `dt = '2024-01-01'` and `y = 24` each find rows in MySQL that comparing the stored form would not — while a bound `?` is normalized for DATE, DATETIME and TIMESTAMP columns. Bound TIME and YEAR comparisons remain refused. An `ENUM` or `SET` member spelled the way it was declared is compared for sameness; a member spelled another way, a number naming a member's position, and any ordering comparison are refused, because MySQL reads each of those by a rule the stored spelling does not meet. Every answer above is pinned to the 8.4.11 golden. |
| Signed `TINYINT` / `SMALLINT` / `MEDIUMINT` / `INT` / `BIGINT` assignment | partial | partial | rejected | planned | partial | [`numeric parser`](parser/lib.rs), [`assignment validator`](frontend/dialect.rs), [numeric oracle case](conformance/cases/p0/numeric-coercion.json), [MEDIUMINT oracle case](conformance/cases/p0/numeric-mediumint.json) | Strict signed ranges are checked before storage for marked columns: `TINYINT` −128..127, `SMALLINT` −32,768..32,767, `MEDIUMINT` −8,388,608..8,388,607, `INT` −2,147,483,648..2,147,483,647, and `BIGINT` `i64::MIN..i64::MAX`. The checked `INSERT`/`UPDATE` path covers parameters, multi-row rollback, triggers, TEMP/attached schemas, reopen, and `VACUUM`; durable DDL and metadata retain the width. String/real coercion, expressions, other widths, permissive warnings, casts, arithmetic, ordering, and protocol errors remain rejected or unimplemented. |
| `SHOW COLUMNS` / `DESCRIBE` / `EXPLAIN table` | partial | partial | experimental | planned | partial | [`checked parser`](parser/lib.rs), [`frontend metadata`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [pinned case](conformance/cases/p0/show-columns.json) | Only plain `SHOW COLUMNS FROM table`, `DESCRIBE table`, `DESC table`, and `EXPLAIN table` — measured on MySQL 8.4.11, `EXPLAIN t` prints exactly what `DESCRIBE t` prints — with MySQL's own synonyms taken, `FIELDS` for `COLUMNS` and `IN` for `FROM`, since a schema reader written against MySQL reaches for either and measured on 8.4.11 all four spellings print the same rows — with one canonical unqualified table or one canonical marked view with a direct projection from one base table, plus an optional single semicolon, are accepted. The selected database is required; database-level `Query` authorization runs before metadata lookup, with an exact table `Select` grant as the narrow fallback. Table metadata comes from verified normalized MySQL DDL and typed defaults, including `PRI` and `auto_increment` for the checked primary auto-increment form. Direct-view metadata verifies persisted view rootpage, SQL, and base-column provenance; it preserves projected type and nullable metadata while clearing table-only `Key`, `Default`, and `Extra`. View chains, projection/source aliases, expressions, joins, qualified or system sources, and duplicate output names are rejected. Frontend metadata preserves declared `INT` versus `INTEGER` spelling, while the wire `Type` column canonicalizes both to `int`. Every type a `CREATE TABLE` here takes reads back, through one renderer shared with `SHOW CREATE TABLE`: a second table of type names had drifted five behind it — `DATE`, `TIME`, `YEAR`, `DOUBLE UNSIGNED` and `FLOAT UNSIGNED` — so a table holding any of them answered 1105 to `SHOW COLUMNS`, `SHOW FULL COLUMNS` and `DESCRIBE` alike while `SHOW CREATE TABLE` printed the same table without complaint. The two are one now, and all thirty-six types are measured on 8.4.11 and matched. Unknown extras fail closed. The pinned case/golden covers this metadata; scan, row, value, packet, and retained-memory bounds apply. A `LIKE` pattern names the columns to report, and `DESCRIBE t <name>` reads a name after the table the same way. `FULL` adds `Collation`, `Privileges` and `Comment`, the first from the stored column collation and the last always empty. `Privileges` reflects database or table grants; column-specific grants remain unsupported. Qualification outside an explicit selected database on `SHOW FULL COLUMNS`, comments, `WHERE`, `DESCRIBE TABLE t`, and a pattern after `EXPLAIN` remain rejected; `information_schema` is not a substitute and remains incomplete. |
| Table-specific `SELECT` grants (persistence and narrow enforcement) | n/a | n/a | partial | partial | partial | [`account store`](server/src/account_store.rs), [`snapshot format`](server/src/account_store_format.rs), [`authorization API`](server/src/authorization.rs), [`persistent store`](server/src/persistent_account_store.rs), [`runtime store`](server/src/runtime_account_store.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`offline provisioner`](offline-provisioner/src/main.rs) | Canonical database/table names, the bounded `select` permission, duplicate/order rules, legacy decoding, durable restart, runtime reload/revocation, and `--table-grant DATABASE.TABLE:select` provisioning are covered in the policy backend/CLI. When database-wide `Query` is denied, the adapter falls back only for parser-confirmed canonical unqualified one-table text or prepared `SELECT`, checks the table `Select` action, and reauthorizes prepared execution against its origin database. Joins, multiple sources, qualified sources, internal catalogs, and unsupported query shapes do not use the fallback. SQL `GRANT`/`REVOKE` is limited to table `SELECT` for exact `@'%'` accounts; wider grant forms and catalog filtering beyond the selected-database narrow path remain open; the final recorded privileged Linux gate passed the table-grant selector. |
| Unsigned integers and `DECIMAL` | partial | partial | partial | partial | partial | [D004 plan](../docs/mysql-compatibility-plan.md), [`DECIMAL` parser](parser/lib.rs), [`exact numeric core`](../core/numeric/decimal.rs) | Fresh `DECIMAL(p,s)` and unsigned columns use exact blobs with declared scale, half-away-from-zero assignment rounding, precision errors, indexed comparisons and ordering, exact `SUM`/`AVG`, and text/prepared output. Projection arithmetic takes a known decimal column or aggregate with a numeric literal through `+`, `-`, `*` or `/`, and known integer columns or aggregates through `+`, `-` or `*`. DECIMAL with a FLOAT/DOUBLE column is refused until mixed precision is implemented. `UPDATE` and `ON DUPLICATE KEY UPDATE` arithmetic with a DECIMAL target keep written and bound decimal operands exact; division by a written nonzero integer is exact. Zero and reversed division and nested or untyped SELECT decimal forms fail closed. Old binary64 decimal tables cannot recover their digits and must be re-imported. Full-range `BIGINT UNSIGNED` storage, indexed comparisons, prepared values, and binary results are covered; mixed signed comparisons and some expression forms remain refused. |
| `utf8mb4_0900_ai_ci` comparisons | partial | partial | partial | partial | partial | [frozen UCA9 weights](../core/translate/mysql_uca9.rs), [data generator](../core/translate/generate_mysql_uca9.py), [license](../licenses/core/unicode-data-license.md), [collation oracle case](conformance/cases/p0/collation-utf8mb4-0900-ai-ci.json) | New v3 text tables use frozen Unicode 9 primary weights for comparison, sort keys, equality hashes, indexes, uniqueness, and `LIKE`. Explicit `utf8mb4_bin` comparisons use byte order with PAD SPACE. The UCA weight data and schema version are fixed so reopening a new table preserves its ordering. Existing v1/v2 text tables need a rebuild and fail closed; unsupported collation forms also fail closed. |
| `utf8mb4_unicode_ci` comparisons | partial | partial | partial | partial | partial | [frozen UCA 4.0.0 weights](../core/translate/mysql_uca400.rs), [data generator](../core/translate/generate_mysql_uca400.py), [license](../licenses/core/unicode-data-license.md), [Laravel and Prisma tables](server/src/frontend_adapter/tests/unicode_collation.rs) | A column or table declared `utf8mb4_unicode_ci` compares, sorts, hashes, indexes, keeps keys unique and matches `LIKE` under Unicode 4.0.0 primary weights with PAD SPACE, checked against MySQL's `WEIGHT_STRING()` for every BMP character. `FIELD`, `GREATEST`, `LEAST`, `NULLIF`, and ordering by or comparing a text-answering call over one, are refused. |
| `SET foreign_key_checks` | yes | yes | n/a | n/a | yes | [`setting reader`](parser/session_settings.rs), [`session variables`](server/src/session_variables.rs), [oracle case](conformance/cases/p0/session-foreign-key-checks.json), [P0 manifest](conformance/Makefile) | The switch is really turned: the engine has the same one, so a row written while it is off may name a parent that is not there. `0`, `1`, `OFF` and `ON` are all taken, under the bare and `SESSION` spellings, and `SELECT @@foreign_key_checks` reads it back. Turning it back on leaves a row written while it was off where it is, which is what MySQL does. A value that is neither is refused where MySQL answers 1231. A connection `USE` opens afterwards is given the switch. |
| `AUTO_INCREMENT` / `LAST_INSERT_ID()` | partial | partial | partial | partial | experimental | [`checked parser`](parser/lib.rs), [`schema envelope`](frontend/schema_sql.rs), [`durable range primitive`](../core/storage/auto_increment.rs), [sequential](conformance/cases/p0/auto-increment.json), [parallel](conformance/cases/p0/auto-increment-parallel.json), [restart](conformance/cases/p0/auto-increment-restart.json), [key clause](conformance/cases/p0/create-counted-key-clause.json), [foreign key](conformance/cases/p0/create-counted-foreign-key.json), [bigint](conformance/cases/p0/create-bigint-counter.json) oracle cases | The checked v3 form accepts exactly one `INT`/`INTEGER`/`BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY`, with `INT UNSIGNED` and `BIGINT UNSIGNED` spellings. The key may be written on the column or as a `PRIMARY KEY (col)` clause of its own, the spelling a dumped schema carries, and the column's attributes may come in any order — Django writes `bigint AUTO_INCREMENT NOT NULL PRIMARY KEY` — as MySQL takes them, measured on 8.4.11. Signed and `INT UNSIGNED` keys use a non-`sqlite_sequence` rowid alias; `BIGINT UNSIGNED` uses a separate `mysql_uint64` primary key, and is creatable, reopenable, and replayable through the identity-backed embedded frontend. Registry-selected embedded sessions reserve one durable contiguous range at execute time for unqualified INSERTs with an explicit non-ID column list and VALUES rows of direct literals and the clock readings an ordinary INSERT takes, and for `INSERT ... SELECT` copies, which reserve the batches of numbers MySQL spends on them. Prepared execution additionally accepts bare `?` values in that same omitted-ID `VALUES` shape: preparation does not reserve, and execution rechecks identity and the triggers the insert sets off before reserving, injecting, repreparing, binding, and writing. Rollback and failed execution do not reclaim a durable range; the first generated ID is recorded only after a successful write and remains connection-local across failure and rollback, including across `USE` database switches. The checked `SELECT LAST_INSERT_ID()` path reads that live state through embedded and current protocol SELECT paths. Narrow text and prepared protocol INSERT paths return affected rows and the first generated ID in their OK packets. A marked table takes an `ALTER TABLE` that leaves its counted column alone, and the column keeps the type it was declared with across one: measured on 8.4.11, a `bigint` key is still a `bigint` after a column is added, placed, restated, renamed or dropped, and an `int unsigned` one still `int unsigned`. Named or numbered markers, expressions, explicit allocator columns, qualified names, `TEMPORARY`, wider INSERT forms, explicit exhaustion handling, and direct connections without an allocator capability remain gated. |
| Checked one-table `UPDATE` | partial | partial | experimental | partial | experimental | [`checked parser`](parser/lib.rs), [`frontend affected rows`](frontend/session.rs), [`core changed-row counter`](../core/connection.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | One unqualified table with no alias, joins, `FROM`, optimizer hints, `RETURNING`, or conflict clause. `ORDER BY` and `LIMIT` are supported via a rowid subquery over integer columns; bare `LIMIT` without `ORDER BY` and non-integer ordering are rejected. Assignment values and predicates use the existing conservative DML forms. Text and prepared protocol execution return bounded OK results. The default affected-row count is rows whose stored key or record changed. `CLIENT_FOUND_ROWS` reports predicate-matched rows instead. Core updates this separate success-only counter for both WAL and MVCC execution, without changing SQLite `changes()`. Multi-table and wider expression forms remain rejected. |
| Classic packet framing and handshake | n/a | n/a | experimental | experimental | partial | [`mysql/server`](server/src/lib.rs), [`connection state`](server/src/connection_state.rs), [`complete-frame owner`](server/src/orchestrator.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`TCP connection foundation`](server/src/runtime_tcp_connection.rs), [`TCP server`](server/src/runtime_tcp_server.rs), [`Unix server`](server/src/runtime_unix_server.rs) | Bounded codecs, stream boundaries, atomic response batches, and a transport-neutral complete-frame owner exist. Result sets reject a column count above the protocol limit before text or binary encoding. The packet writer bounds batch staging by queued frame and byte limits and leaves the queue unchanged when a batch is rejected. The same-UID Unix boundary drives it as an already-secure transport without advertising `CLIENT_SSL`; the supervised TCP server owns the bounded accept/reaper lifecycle and the crate-private TCP owner performs the mandatory TLS transition before authentication. The standalone runtime exposes a TCP CLI whose `--listen IP:PORT` mode requires both `--tls-cert PATH` and `--tls-key PATH` and conflicts with Unix socket flags; the checked-in privileged `mysql_async` TCP E2E is wired into CI, and the final recorded privileged Linux gate passed it. Global connection authorization and optional authorized initial-database selection must succeed before fast/full authentication emits its final OK; failure emits a fixed 1045 ERR and closes. Once a client has signed in, a command's payload may be up to 67108863 bytes, the longest MySQL 8.4 takes under its default `max_allowed_packet` of 64 MiB, which `@@max_allowed_packet` and `SHOW VARIABLES` read back as 67108864. A payload of 0xFFFFFF bytes or more arrives split into full packets and a shorter last one, empty when the payload is an exact multiple; it is joined as its bytes arrive rather than by the length a header declares, and answered from the sequence number after its last packet — measured on 8.4.11, a query of exactly 0xFFFFFF bytes, sent as packets 0 and 1, is answered from 2. Measured on 8.4.11, a payload that would reach 67108864 bytes is answered 1153, SQLSTATE `08S01`, `Got a packet bigger than 'max_allowed_packet' bytes`, as soon as the header of the packet that takes it that far arrives, numbered after that packet, and the connection is closed without reading the rest; this server answers the same, where it used to close at 16 KiB without an answer. Before a client has signed in, a packet is held to 16 KiB and never split. A result row goes out split the same way when it is 0xFFFFFF bytes or longer, which it may now be: a row is no longer held to 4,096 bytes, one result holds up to 64 MiB, and an answer longer than the runtime's whole write queue (`--max-write-bytes`, which may now be set up to 128 MiB, and `--max-write-frames`) is answered 1235 in place of its rows instead of closing the connection. `COM_STMT_SEND_LONG_DATA` takes up to 64 MiB for a connection's parameters together and past that answers the `COM_STMT_EXECUTE` that follows with 1105 and MySQL's message, `Parameter of prepared statement which is set through mysql_send_long_data() is longer than 'max_allowed_packet' bytes`; measured on 8.4.11, MySQL holds each parameter to 67108864 bytes rather than all of them together. `SET [SESSION] max_allowed_packet`, in every spelling, answers 1621 with MySQL's message, the session's value being read-only there too; `SET GLOBAL` is refused. Decoder feeds emit at most 16 packets at a time without rejecting a larger valid coalesced read, and accepted response-packet limits are at least 4,096 bytes. |
| `caching_sha2_password` | n/a | n/a | experimental | experimental | partial | [`verifier`](server/src/verifier.rs), [`offline provisioning`](server/src/offline_provisioning.rs), [`offline CLI`](offline-provisioner/src/main.rs), [`checkpoint authority`](checkpoint-authority/src/lib.rs), [`runtime account store`](server/src/runtime_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`TCP connection foundation`](server/src/runtime_tcp_connection.rs), [`TCP server`](server/src/runtime_tcp_server.rs) | Constant-time verification mints an opaque principal only after success. The persistent Unix store retains one bounded, CAS-published generation with full verifiers, retired IDs, global privileges, and canonical database grants; open and reload require the exact external store-ID/revision/digest checkpoint. The Unix-only CLI initializes or adds one account through a durable journal, accepts canonical `--database-grant` permissions and validated `--table-grant DATABASE.TABLE:select` options, and reconciles both initialization and replacement journals. `add-account` rebuilds a pinned authority-approved generation and publishes only if its memory and disk snapshot still match. Crash-safe initialization, addition, and reconciliation require a client bound to the journal authority ID; mismatch fails before writes. Replacement recovery retries only exact expected-to-replacement transitions and retains ambiguous evidence. Initialization and account addition have four-boundary process-kill coverage; initialization has the sixteen-point publication-fault matrix; every replacement snapshot-publication syscall point has fault coverage; and journal removal has unlink/directory-sync fault plus crash-inside-unlink coverage. Same-effective-UID and privileged cross-UID real-authority gates add a granted account and verify exact revision one; the former also reloads, restarts, reconciles an ambiguous durable replacement, and kills initialization and addition at all four durable boundaries before recovery. Full authentication is wired over the same-UID Unix transport, and the supervised TCP server routes its accepted streams through the mandatory TLS/authentication path. V1 is exact username-only. Account/grant edits or removal and distinct-UID crash-boundary recovery remain missing; the checked-in TCP E2E and cert/key loader checks are present, and the final recorded privileged Linux gate passed the TCP selector; broader certificate/trust deployment policy remains open. |
| `COM_QUERY` | partial | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs) | Checked `SELECT`, the conservative schema-DDL subset including `DROP TABLE`, and ordinary `INSERT`, `DELETE`, and one-table `UPDATE`, which return bounded OK results with affected-row counts; the narrow generated-ID `INSERT` also returns its first generated ID. UPDATE reports changed rows by default and matched rows after `CLIENT_FOUND_ROWS` negotiation. Strict `CREATE DATABASE`, `ALTER DATABASE`, `DROP DATABASE`, `SHOW CREATE DATABASE`, `USE`, and `SHOW DATABASES` remain available. Other statements are rejected. A selected database is reauthorized for every ordinary query; an unselected ordinary query returns 1046 without a policy lookup. Admin authorization happens before catalog access. Each checked write carries one query deadline across its stages, checks it between blocking catalog and allocator operations, and gives Core SQL execution only the remaining time. A synchronous blocking I/O operation cannot yet be interrupted in progress. An observed timeout returns MySQL error 3024 and leaves the connection usable. |
| `COM_PING` / `COM_QUIT` | n/a | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs) | Transport-neutral dispatch and a real same-UID Unix worker path are covered. |
| `COM_INIT_DB` | n/a | n/a | experimental | n/a | partial | [`frontend adapter`](server/src/frontend_adapter.rs), [`DatabaseCatalog`](frontend/database_catalog.rs), [`persistent account store`](server/src/persistent_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs), [`Unix runtime`](runtime/src/main.rs) | The Unix adapter canonicalizes and authorizes before the shared catalog, preserves the old selection on failure, returns fixed 1045 for denied or unavailable policy, and returns 1049 only for an authorized unknown name. The same-UID worker wires this path for both handshake selection and `COM_INIT_DB`; the standalone runtime owns the blocking accept loop and worker reaper. |
| Prepared commands | partial | partial | n/a | partial | partial | [`mysql/frontend`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`statement execute`](server/src/statement_execute.rs), [`response`](server/src/response.rs), [D012 core contract](../core/dialect/mod.rs) | `COM_STMT_PREPARE`/`EXECUTE`/`RESET`/`CLOSE` support checked `SELECT` and conservative ordinary `INSERT`/`UPDATE`/`DELETE`; SELECT results use binary rows and writes return OK effects. Binary parameter decoding, cached parameter types, schema reprepare snapshots including refreshed static metadata for checked literal projections and single-wildcard expansion; multiple wildcards fall back to all-generic metadata. Declared protocol metadata widths for `TINYINT`, `SMALLINT`, `MEDIUMINT`, `INT`/`INTEGER`, and `BIGINT` are covered, along with checked signed Int8/Int16/Int24/Int32/Int64 result primitives. `MEDIUMINT` uses column length 9; its 24-bit signed range −8,388,608..8,388,607 is encoded as a fixed four-byte little-endian `MYSQL_TYPE_INT24` value. Known declared result types are normalized case-insensitively, unknown declarations fall back to inferred metadata, and untyped `NULL` expressions remain untyped. Signed `MYSQL_TYPE_LONGLONG` tests cover `i64::MIN`/`i64::MAX` without unsigned reinterpretation. `COM_STMT_SEND_LONG_DATA` appends binary or text chunks without a response, retains at most 64 MiB per connection, defers errors until execute, and clears staged data on execute, successful reset, or close; unknown statement IDs drop staged long data. The AUTO_INCREMENT case is limited to the documented omitted-ID bare-`?` `VALUES` form. A statement with no parameter that the checked path does not prepare and that answers no rows — beginning `ALTER`, `CREATE`, `DROP`, `RENAME`, `TRUNCATE`, `SET`, `LOCK`, `UNLOCK` or `FLUSH` — is retained and run through the text path when it is executed, which is how Laravel, preparing every statement, creates its tables; such a statement's own errors surface at execution rather than at preparation. A `SELECT` of `DATABASE()`, `VERSION()`, system variables and the session calls beside them — Laravel's `select version() as version, database() as db` and its `@@character_set_client as client, ...`, Prisma's `SELECT @@version, @@GLOBAL.version` — is answered from the session when it is executed, as it is over text: measured on 8.4.11, MySQL answers the same columns over both protocols, a word as a `VAR_STRING` and a whole number as a `LONGLONG`, and a variable it has not got is 1193 at preparation. `DATABASE()` after the `FROM` of an `information_schema` query is written in as the selected database before a statement is prepared, as it is for text. Cursor modes, exact long-data error diagnostics, prepared transaction commands, and wider SQL remain rejected. |
| Prepared statement quota | n/a | partial | n/a | experimental | partial | [`authority`](frontend/session.rs), [`runtime config`](server/src/runtime_config.rs), [`response`](server/src/response.rs) | The committed authority (`9f073b116`) uses default `16,382`, inclusive range `0..=4,194,304`, and zero to disable new prepares; runtime CLI/listener enforcement is committed in `d8abd505b`. Shared-capability connections count retained statements together; failed prepares release permits, and close, successful connection-level reset, successful close, or drop releases retained permits. `COM_STMT_RESET` keeps the statement and only clears bindings. Exhaustion maps to error `1461` / SQLSTATE `42000`, while statement-ID exhaustion remains separate. Five privileged runtime E2E tests remain ignored; the final recorded privileged Linux gate passed the quota selector. |
| `COM_RESET_CONNECTION` | n/a | n/a | experimental | n/a | partial | [`connection state`](server/src/connection_state.rs), [`dispatcher`](server/src/dispatcher.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`mysql_async` Unix E2E](runtime/tests/unix_e2e.rs) | Command `0x1f` accepts an empty body, rolls back before restoring autocommit, clears prepared statements and pending long data, resets `LAST_INSERT_ID()` to zero, keeps the selected database, returns OK, and remains in `Ready`. A rollback failure stops cleanup and leaves the remaining state unchanged. The privileged Linux pool E2E is ignored by default; the final recorded privileged Linux gate passed the pool selector. |
| TCP/TLS and Unix-socket listeners | n/a | n/a | planned | planned | partial | [`runtime config`](server/src/runtime_config.rs), [`runtime TLS loader`](server/src/runtime_tls.rs), [`runtime Unix listener`](server/src/runtime_unix_listener.rs), [`TCP listener foundation`](server/src/runtime_tcp_listener.rs), [`TCP connection foundation`](server/src/runtime_tcp_connection.rs), [`TCP server`](server/src/runtime_tcp_server.rs), [`reload supervisor`](server/src/runtime_account_reload_supervisor.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs), [`Unix socket filesystem`](server/src/unix_socket_fs.rs), [protocol architecture](../docs/mysql-compatibility-mode.md) | The blocking Unix boundary limits a pathname to 103 raw bytes, accepts Linux `SO_PEERCRED` or macOS `getpeereid` peers only when their effective UID matches startup, and rejects other Unix targets. It descriptor-walks from root without following symlinks, requires every ancestor to be root- or effective-UID-owned and not group/other-writable, rejects sticky writable directories, requires final `0700`/effective-UID ownership, holds a `0600` owner lock, rejects every pre-existing endpoint including stale sockets, rechecks the exact checkpoint and catalog before bind, publishes a `0600` endpoint, and removes it only when its retained identity still matches. A post-bind identity failure retries owner/type-checked cleanup; inability to confirm cleanup returns an explicit operator-inspection error. RAII connection/admission limits plus authentication, idle, query, write, checkpoint, and shutdown deadlines apply; degraded account state blocks before and after accept. The listener owns one joinable periodic reload worker. Its first tick waits for the interval and each next tick waits after completion, avoiding overlap and backlog; explicit reload stays available and serializes with it. A failed scheduled tick retains existing-session authorization but blocks new admission until a later exact reload recovers it. Idempotent shutdown wakes blocked accepts and the reload worker or checkpoint wait, stops later handoff registration, signals every handoff that linearized first, performs bounded drain under one shared deadline, reports reload status as `Stopped`, `TimedOut`, or `Failed`, and retries a timed-out reload join later. The reload worker's `Drop` may block to avoid detaching it, and panic fails closed. The owner checks lifecycle before greeting and each decoded frame, preventing a buffered command from starting after shutdown; Core work already started is bounded by query timeout rather than asynchronously cancelled. Pathname bind and checkpoint validation are not one atomic operation; the remaining replacement threat is inside the declared same-effective-UID trust boundary. `RuntimeUnixServer` supplies the blocking run-once accept loop, bounded worker-event queue, and one joinable reaper; completion-before-registration and thread-exit-safe joins are covered. Ordinary worker errors are counted and redacted without stopping accept, while worker panic, account-reload-owner failure, and listener, spawn, or reaper infrastructure failure fail closed. Account-not-ready waits without spinning, and explicit reload plus readiness are forwarded. Shutdown uses one shared deadline, retains timed-out handles for later retries, and `Drop` joins without a time limit. Endpoint cleanup remains identity-safe and the Unix listener remains same-effective-UID. The TLS material loader validates trusted no-follow paths, certificate/key ownership and modes, 1 MiB file bounds, PEM labels, key count, certificate/key pairing, and an explicit rustls TLS 1.2/1.3 server policy. The supervised `RuntimeTcpServer` owns the bounded TCP accept/reaper lifecycle, explicit shutdown/retry, worker panic/error accounting, and lost-reaper worker retention; it routes accepted streams through the mandatory SSLRequest/rustls/authentication owner. The standalone `turso-mysql-server` CLI accepts `--listen IP:PORT` only with both `--tls-cert PATH` and `--tls-key PATH`, and rejects mixing TCP and Unix listener flags. The checked-in privileged TCP `mysql_async` E2E validates a configured client CA and `localhost`, rejects wrong-hostname, missing-CA, and plaintext clients, and checks port release after `SIGTERM`; CI wires the selector, and the final recorded privileged Linux gate passed both driver selectors. Broader certificate/trust deployment policy remains open. |
| Driver and ORM compatibility | partial | n/a | experimental | partial | partial | [D010/P6 plan](../docs/mysql-compatibility-plan.md), [`mysql_async` Unix E2E](runtime/tests/unix_e2e.rs), [`mysql_async` TCP E2E](runtime/tests/tcp_e2e.rs), [exact CI selector](../scripts/test-checkpoint-authority-cross-uid.sh) | The experimental external-driver pilot pins `mysql_async = "=0.37.1"`. Its ignored privileged Unix E2E uses default `OptsBuilder` values (no explicit `max_allowed_packet` or `wait_timeout`) and covers authentication, `USE`, text DDL/DML, prepared DML, reads, independent connection state, reconnect, pool reset, and `SIGTERM` cleanup. A separate ignored privileged TCP E2E uses a private CA and `localhost` hostname validation, rejects wrong-hostname, missing-CA, and plaintext clients, and checks `SIGTERM` port release. CI also selects pinned Connector/J 9.6.0 and go-sql-driver/mysql 1.9.3 over verified TLS/TCP, including prepared CRUD, rollback, schema inspection, and two text-query result sets. The privileged Linux gate passed these pinned driver checks on 2026-09-26 and also passed the pinned GORM and Hibernate fixtures described below; other versions, settings, and metadata paths remain open. |


Pinned GORM 1.31.2 with `gorm.io/driver/mysql` 1.6.0 and
`go-sql-driver/mysql` 1.9.3 passed a disposable MySQL 8.4.11 oracle and the
privileged cross-UID TLS/TCP gate. It exercises `AutoMigrate` on new and
existing tables, migration up/down, table/index/column/type inspection, CRUD,
relations, exact `DECIMAL`, NULL, whole-second timestamps, unique and foreign
key errors, transaction commit/rollback, and pool reopening. Two tables share
one logical index name. Hibernate ORM 6.6.0.Final with Connector/J 9.6.0
passed the same oracle and gate. Its `create-only`, `update`, `validate`, and
`drop` phases inspect schema through JDBC metadata, test relations and values,
and validate after a server restart. Hibernate uses
`useInformationSchema=false`; the other Connector/J metadata path and wider
ORM mappings remain unverified. Both fixtures keep decimal values exact and
limit timestamps to whole seconds; fractional timestamps now have separate
parser, frontend and protocol regression coverage.
The MySQL 8.4.11 comparison was a manual snapshot; CI runs the pinned Turso
fixtures but does not run a MySQL differential gate.

## Verification snapshot

A MySQL 8.0.46 command-line client E2E is checked in under the privileged
Linux cross-UID fixture. CI builds its fixture image from the pinned Ubuntu
base with exact MySQL client packages, then runs the CLI over verified TLS/TCP
against a disposable database. The test checks schema creation and inspection,
data writes and reads, rollback, and a second connection. The pinned CLI
selector and the full cross-UID gate passed locally on 2026-09-26 using the
Linux x86_64 fixture under Docker Desktop. This verifies the tested commands
and client version; broader command-line compatibility is still open.

Connector/J 9.6.0 and `go-sql-driver/mysql` 1.9.3 are pinned in the same
fixture. Both passed verified TLS, prepared inserts, CRUD, rollback, `ALTER
TABLE ADD COLUMN`, and schema inspection. The JDBC fixture sets
`useInformationSchema=false`, so `DatabaseMetaData.getColumns` reads `SHOW FULL
TABLES` and `SHOW FULL COLUMNS` with an explicit selected database. Its default
`information_schema` metadata query uses expressions and columns this server
does not yet implement. The Go fixture uses `mysql.NewConfig()` to keep the
driver's default packet size. The gate passed locally on 2026-09-26.

The two fixed drivers also opt into multiple statements (`multiStatements`
for Go and `allowMultiQueries` for Connector/J) and read both result sets from
`SELECT 1; SELECT 2`. The server negotiates `CLIENT_MULTI_STATEMENTS`, keeps
response sequence IDs continuous, and marks every non-final result with
`SERVER_MORE_RESULTS_EXISTS`. The dispatcher refuses more than 32 statements
before executing any of them. It closes the connection if the assembled
response exceeds 512 frames or 1 MiB; earlier statement effects may already
have happened. A runtime write-queue setting can impose a smaller bound.

Account administration has a deliberately narrow SQL surface: `CREATE USER
'name'@'%' IDENTIFIED BY 'password'`, `GRANT SELECT ON db.table TO
'name'@'%'`, and the matching `REVOKE`. The offline provisioner must explicitly
bootstrap an account with `--global-manage-accounts true`. The authorization
decision is repeated against the exact externally checkpointed generation
used for the journaled update and CAS, then the running account store reloads
before a SQL success response. A table `SELECT` grant also permits selecting
its database while it remains in force; it does not grant other table access.
Other account hosts and grant types remain unsupported.

The handshake now announces `8.0.36-turso`. Connector/J treats `8.0.0` as an
older branch and requests removed query-cache variables before opening a
connection. The announced number selects the modern MySQL 8 driver path; it
does not claim full MySQL 8.0.36 SQL compatibility. Connector/J also sends
`SET character_set_results = NULL`, which disables conversion here and reads
back as NULL through `SELECT @@character_set_results` (an empty value through
`SHOW VARIABLES`), matching the measured MySQL 8.4.11 behavior.

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
label in durable MySQL DDL, takes the trailer MySQL prints after every table and rejects every other
engine/table option. An ordinary-PK
table is rewritten through the same renderer every other table's is, so an
`ALTER TABLE` runs against one, and so does a counted table, whose renderer is
told which column it counts on. The
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
broader driver/ORM compatibility, and the P7 release gate remain open. The protocol
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
