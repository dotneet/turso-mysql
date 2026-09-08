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
`VARBINARY` and a `BLOB` included. The comment is empty, which is the only
comment a column here can have — the option is refused where a table is created.

`Privileges` is answered NULL, and that is a difference worth saying out loud.
MySQL reports the connected user's grants on the column, `select,insert,update,
references` for one that may do everything. This server's grants are per
database and per table rather than per column, so it does not keep that figure,
and NULL says so where a made-up list would claim something. The column is
nullable in MySQL too, so NULL is a value a client can read; what it must not do
is read it as "no privileges".

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

One difference goes with the names rather than with the keys. An index name is
per table in MySQL and database-wide in the engine, so two tables cannot carry
an index of the same name here — measured, MySQL takes `CREATE TABLE one (id
INT, KEY (id))` and `CREATE TABLE two (id INT, KEY (id))` both, and the second
is refused here because `id` is already an index name. It reaches a written name
as readily as an unnamed one; what unnamed keys change is how easily it is met,
since a column called `id` or `name` is in many tables. Naming the engine's
index after the table it belongs to is what this needs, and that is a change to
what existing databases already store.

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

`ORDER BY` on a text column asks for the same collation, so a query that filters
without regard to case orders that way too: `abc` and `ABC` sort together rather
than every uppercase name sorting first. And a `?` against a text column binds a
string, compared the same way.

Both needed the same thing. Only the frontend can see a column's type, so the
parser is told which columns are text and the statement is rendered again — and
only a statement that orders by a bare column or compares against a `?` is
rendered twice, since those are the two places the rendering depends on it. A
comparison records whether it was rendered with the collation, so the value
check and the rendering cannot disagree: a string parameter is taken exactly
where the SQL asked for the collation, and refused where it did not.

The collation is asked for only where text actually meets the column, never on
an integer comparison, because a collation an index does not carry stops the
planner from using that index. One thing follows from putting it in the rendered
SQL: a string against an integer column is refused, where MySQL coerces the
string.

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

The same three readings are written as values, which is how a row records when it was made:
`INSERT INTO t (created_at) VALUES (NOW())`, `INSERT ... SET d = CURRENT_DATE`, an
`ON DUPLICATE KEY UPDATE updated_at = NOW()`, and `UPDATE t SET dt = NOW()` all write it.
What lands in the column is then put into the form that column holds, the way a written value
is: measured on 8.4.11, `NOW()` into a `DATE` keeps the day and `CURDATE()` into a `DATETIME`
becomes that day's midnight, and both do here. A moment written into a word is the moment
written out, nineteen characters of it, and one too wide for the column is refused with 1406
the way any oversized value is. One difference: MySQL raises 1292 for the time it drops going
into a `DATE` and this drops it quietly. A moment written into a number is refused here, where
MySQL runs it together into 20260908170430 — reading a moment as a number is a rule of its
own and it has not been measured beyond that one shape.

`UPDATE t SET n = n + 1` counts a column up, which is what a `SET` is most often asked to do.
A column is read in an assignment, and `+`, `-` and `*` over one, nested as deeply as they
are written. Division is not: measured on 8.4.11, `b / 2` over 101 answers 50.5 there and 50
in the engine, so the two would write different numbers. Counting a column past its range is
refused the way any oversized value is, and the row keeps what it had — MySQL answers 1690
for the same statement.

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

The body has to read one table and project its columns, for the reason a CTE's body does: a
wildcard or an expression leaves no name to resolve an ordinal through, so
`(SELECT SUM(n) AS total FROM t) x` is refused. So is a `LATERAL` one, one naming its own
columns, and one in an `UPDATE` or a `DELETE`, each of which reads its own table.

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

`CONCAT(name, '-', id)` is how a query builds a label out of a row, so the call takes a number
as readily as a word. Its answer is as wide as its arguments laid end to end, measured on
8.4.11, and a number spells as many characters as its type does rather than as many as its
column reports: a `BOOLEAN` column reports one and spells four, being a `TINYINT` under the
display width MySQL keeps for it. A moment, a day, a span of time and a year are spelled the
way they are stored, so those are taken too.

A `DECIMAL`, a `FLOAT` and a `DOUBLE` are refused. What lands in the answer is the number
spelled out, and MySQL spells those its own way: measured, a `DECIMAL(10,2)` holding 1.50
spells `1.50` where the engine spells `1.5`, a `FLOAT` holding a third spells `0.333333`, and
a `DOUBLE` holding 12345678901234567890 spells `1.2345678901234567e19`. Answering a different
string would be worse than refusing the shape.

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
a rule over its branches rather than a type of its own, measured on 8.4.11: a `LONGLONG` as
wide as its widest branch plus one for the sign — `THEN 1 ELSE 0` reports 2, `THEN 100 ELSE -5`
reports 4, `THEN n ELSE 0` over an `INT` reports 11, which is the `INT`'s own ten digits and
the sign, and `IF(c, n, big)` reports 20. It carries the binary and numeric flags and no
decimal places, and is NOT NULL only when every branch is and there is an `ELSE` for a row to
fall to — measured, a `CASE` with no `ELSE` is nullable whatever its branches hold.

A branch is a written number or a column and nothing else. A branch carrying a scale is
refused: measured, `THEN 1.5 ELSE 0` answers a NEWDECIMAL, which is a rule of its own. So is a
word branch beside a number branch, which is a coercion.

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
table takes them. The counter is raised past the highest id the statement wrote before the row
is written, so it never hands the same number out again. Measured on 8.4.11 and matched: rows
written out of order still leave the counter one past the highest of them, a written id below
the counter leaves it where it stands, and a negative id is stored as written and moves
nothing. `LAST_INSERT_ID()` is left as it stood, which is what MySQL does — but the id the
statement reports to its client is the last row's written value, a different number from the
one the counter moved past when the rows descend. MySQL's `INSERT ... SET id = 1` form writes
the same row and is taken the same way, and writing an id that is already there is the
ordinary collision, 1062.

Writing a 0 or a NULL into that column is refused. Both ask MySQL for the next number rather
than writing one, and so does a statement that writes some rows and counts others; the counter
is raised once for the whole statement here, which cannot be done for some of its rows and not
the rest.

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
count is a non-null `LONGLONG` of length 21 whatever it counts. `MIN`, `MAX`, `SUM`, `AVG`
and the rest answer their argument's own type, so each still takes a bare column: reading the
table a qualifier names is work the result metadata has not been taught.

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
engine holds an integer as an `i64`. `DECIMAL` carries a scale the engine does not keep, and
for the same reason a `DECIMAL` column is not written out with `CHAR`: `1.50` would come back
as `1.5`. A `DOUBLE` prints by a rule of its own. `CHAR(n)` cuts the value short and warns,
and so does reading a number or a day out of a word — `CAST('  7 apples' AS SIGNED)` is 7
there with a warning this does not raise.

`CONVERT(col, <type>)` means what `CAST(col AS <type>)` means and is written out the same
way. The other spellings are refused: `CONVERT(col USING <charset>)` names a character set
rather than a type and this server speaks one, and the T-SQL `CONVERT(<type>, col)` and
`TRY_CONVERT` write the two the other way round and answer NULL where MySQL raises.

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

A `JSON` column is not compared, and that is a decision rather than a gap. MySQL does not
compare a `JSON` column to a written document: measured on 8.4.11 against a row holding
`{"a": 1, "b": 2}`, `doc = '{"a": 1, "b": 2}'` finds **nothing**, and neither does
`doc = '{"b":2,"a":1}'`. What it compares is the written value read as a JSON *string* —
so `doc = 'word'` finds the row holding the JSON string `"word"` while `doc = '"word"'`
finds nothing — and it orders by JSON's own type precedence, which puts every object and
array above every string: measured, `doc > '[1, 1]'` finds all three rows. Comparing the
document this stores would answer the opposite in every one of those, so the comparison is
refused rather than answered differently.

A `BLOB` column is not compared yet, and that one is a gap. Measured: MySQL compares the
bytes, so `payload = 'ABC'` finds no row holding `abc` and `payload > 'a'` reads them in byte
order — which is the engine's own comparison, without the collation a text column asks for.
Taking it needs the renderer to be told a column is binary so it leaves that collation off,
which is the same channel that tells it a column is text.

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
operator. Like `=`, text column comparisons with `<=>` receive `COLLATE NOCASE`
so MySQL's case-insensitivity is preserved, while integer and NULL operands
evaluate without coercion. Both `WHERE col <=> 1` and `WHERE col <=> NULL`
(as well as prepared parameters) evaluate identically to MySQL.

`LIKE` needs no collation of its own. The engine already matches a pattern
without regard to ASCII case, which is what MySQL's default collation does, so
`WHERE name LIKE 'A%'` finds `abc` in both. `NOT LIKE`, `%` and `_` all cross
unchanged, and the column has to be a text column for the same reason a `=`
does. The pattern is bound as readily as it is written, which is what a client
that prepares a search writes: `WHERE name LIKE ?` binds the pattern and needs
no collation, because the engine's matching already ignores case.

Two forms are refused. A pattern holding a backslash, because MySQL reads one as
an escape and the engine reads it as a byte, so `'a\%'` would match a different
set of rows in each — a bound pattern is held to that where it arrives, since
there is no text to read until it binds. And an explicit `ESCAPE`, which has
nowhere to go while the backslash question is open. The accent half diverges
here exactly as it does for `=`.

The scalar calls taken so far are `LOWER`, `UPPER`, `REVERSE`, `REPEAT`,
`REPLACE`, `LPAD`, `RPAD`, `INSTR`, `LOCATE` (2 arguments), `HEX` (text columns),
`LENGTH`, `CHAR_LENGTH` (and its `CHARACTER_LENGTH` spelling), `NOW()` with
`CURRENT_TIMESTAMP`, `ABS`, `SIGN`, `SQRT`, `POW` (with `POWER`), `MOD`, `ROUND` over one argument,
`GREATEST`, `LEAST`, `NULLIF`,
`IFNULL` with `COALESCE`, `CONCAT`, and `LEFT` with `RIGHT`. A `CASE` and its call spelling `IF` are taken
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
`ROUND`, `FLOOR`, `CEIL`, `CEILING`, and `SIGN` answer a `LONGLONG` of length 21 however wide the argument was (and carry `NOT NULL` when their column does),
`SQRT` and `POW` answer a `DOUBLE` of length 23 and not-fixed decimals (31),
`GREATEST` and `LEAST` take 2 or more homogenous arguments (all integers or all text) and answer the widest width (e.g. `LONGLONG` 11 for integer, or `max_len * 4` for text), preserving `NOT NULL` only if all arguments are non-null;
`NULLIF(expr1, expr2)` answers expr1's shape (widening `INT` to `LONGLONG` 11) with `NOT_NULL_FLAG` cleared since matching arguments yield NULL;
and `IFNULL` keeps the width.
`NOW()` answers a `DATETIME` of length 19. `CONCAT` is as wide as its arguments
laid end to end, a string literal counting the characters it spells, so
`CONCAT(v, 'z')` over that `VARCHAR(8)` reports 36 and `CONCAT(v, v)` 64; `LEFT`,
`RIGHT`, and `SUBSTRING` (or `SUBSTR`) are as wide as the count they were asked for, so `LEFT(v, 2)`
and `SUBSTRING(v, 1, 2)` report 8. A `CASE` is as wide as its widest branch:
`CASE WHEN n > 1 THEN 'y' ELSE 'n' END` reports 4 and is NOT NULL, and
`IF(n > 1, 'y', 'n')` reports the same, measured identically.

Every branch of a `CASE` has to be a string literal or `NULL`, for the width to
be knowable. Two things drop the `NOT_NULL` flag, both measured on 8.4.11: no
`ELSE`, because a row matching nothing answers NULL, and a `NULL` branch. The
width is the widest string branch either way — `CASE WHEN n < 3 THEN 'low' END`
and `... THEN 'low' ELSE NULL END` both report 3 characters and no flag. A
`CASE` whose every branch is NULL is refused, since there is no width left to
answer with. A `CASE col WHEN ...` is refused: it
compares its operand, which raises the coercion question a `WHERE` comparison
raises and has not been measured here. The `WHEN` predicate itself goes through
the same checked path a `WHERE` does, so a comparison inside it is validated
against the column's type.

`ROW_NUMBER()`, `RANK()`, `DENSE_RANK()`, `NTILE(n)`, `PERCENT_RANK()`,
`CUME_DIST()`, `LAG(col)`, `LEAD(col)`, `FIRST_VALUE(col)`, `LAST_VALUE(col)`
and `NTH_VALUE(col, n)` number, rank and shift the rows a `SELECT` answers. Both engines
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
and a `LAG` or `LEAD` carrying an offset or a default is refused, bringing rules
of its own.

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

One divergence goes with it, and it is metadata rather than data: the engine
answers every column of a windowed statement out of its own sorter, so the
other columns lose the table they came from and the key flags that go with it.
`SELECT id, ROW_NUMBER() OVER (ORDER BY n) FROM w` reports `id` against no table
and with only its numeric flag, where MySQL reports it against `w` with
`NOT_NULL` and `PRI_KEY`. The values are the same; what a client cannot read is
where the column came from, and a column believed nullable when it is NOT NULL
is never wrong in the dangerous direction.

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
be null, which is the whole reason a client writes it. MySQL takes a text call
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
`COLLATE NOCASE` so that MySQL's case-insensitive comparison is respected (e.g.
`'b'` and `'B'` count as one distinct value). The column is named after the call
as written, case kept and the argument unquoted, and an alias replaces that name.

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
`DOUBLE` both answer `DOUBLE` with length 23 and 31 decimals. `GROUP_CONCAT`
answers `MYSQL_TYPE_BLOB` (252) of length 65536 and 31 decimals, flags 0,
skipping NULL values and defaulting to comma `,` separator.

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

A `WITH` clause names a subquery so the statement can read it as a table.
Measured on 8.4.11: a column that comes through a CTE names the CTE as its table
and carries the base column's own type and flags — a primary key stays a primary
key — which is what this reports.

The ordinal a result column carries counts through what the CTE projected, not
through the table. A CTE can project its table's columns in any order, and
resolving straight into the table hands each column the other's metadata; the
projected names are carried for exactly that reason.

Refused: `WITH RECURSIVE`, a body that reads more than one table or carries its
own `ORDER BY` or `LIMIT`, a column list on the name, the materialization hints,
and a body whose projection is not whole columns — a wildcard or an expression
leaves no name to resolve an ordinal through. A `WHERE` comparison against a
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
`DELETE` predicates, where text lists collate under `NOCASE` and three-valued
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

Refused: a subquery projecting more than one column or reading more than one
table, one carrying its own `ORDER BY` or `LIMIT`, and a subquery anywhere but a `WHERE`.

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
query's column keeps its type and length and names no table, and carries none of
the source column's key facts. It also keeps `NOT_NULL` when both branches are
NOT NULL; this drops it either way, because the engine reports only the first
branch's column and cannot tell — a column a client believes may be NULL is
never wrong, where the reverse would be.

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

One length still differs. Measured, a `MIN` over a `TEXT` column reports 1048560
where this reports the column's own 262140; the rule behind that number has not
been worked out.

`GROUP BY` is taken over whole columns, and is held to `ONLY_FULL_GROUP_BY`.
That mode is in MySQL 8.4's default `sql_mode` and this server takes a client's
`SET sql_mode` naming it, so the rule is enforced rather than assumed: every
projection that is not an aggregate or a literal has to be one of the grouping
columns, and one that is not is refused where MySQL answers 1055. A wildcard
projection is refused for the same reason, since the columns it names cannot be
checked against the grouping. The grouping columns keep their own result
metadata and the aggregates keep theirs, exactly as they do without a `GROUP BY`.

`HAVING` comes with it, over the aggregates and the grouping columns. A
comparison on a grouping column goes through the same checked path a `WHERE`
comparison does. One on an aggregate cannot, since there is no column to compare
types against, so the aggregate's own argument column is recorded instead —
`HAVING SUM(score) > 45` holds `score` to the same rule `WHERE score > 45`
would, which is what makes the integer literal safe. `COUNT` records nothing,
because it answers an integer whatever it counts. `AND`, `OR`, `NOT` and
parentheses cross; the right side has to be an exact signed integer.

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
in byte order. So the byte collation drops the NOCASE a text comparison
otherwise asks for, and nothing else changes.

Five shapes are refused. A collation over a `LIKE` is one: the engine matches a
pattern without regard to ASCII case whatever is asked of it, so naming a byte
collation there would be ignored rather than answered — measured, MySQL answers
the one row spelled exactly so. A collation over a membership test is another,
that being written out as comparisons whose collation has not been measured.
The rest are a collation over a number, over a bound value — which carries no
text until it binds — and one from another character set, which is 1253 there.

An ordering may name a collation. Measured on MySQL 8.4.11 over 'beta',
'Alpha', 'alpha', 'Beta', 'Zulu' and 'apple': naming none orders them without
regard to case, `utf8mb4_bin` puts every capital first — which is byte order,
the engine's own — and `utf8mb4_0900_ai_ci` and `utf8mb4_general_ci` each order
them the way naming none does. So the byte collation renders as no collation at
all and the two case-ignoring ones render as the NOCASE a bare text column
already gets. Backwards is the same order reversed, and a collation over a
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
second, dropping whatever is left over. Measured on MySQL 8.4.11 over four
pairs — two and a half days apart, two hours apart across midnight, two days
apart backwards, and one second short of a day — and matched: the backward pair
answers −2 days rather than −3, and the one a second short answers 0 days and
23 hours. Dividing the seconds between the two moments truncates the same way
in the engine, which is what makes the count the same count. Every unit reports
a whole number of length 21, where `DATEDIFF` reports 9.

Only the units of fixed length are taken: SECOND, MINUTE, HOUR, DAY and WEEK. A
month, a quarter and a year are counted by the calendar rather than by their
length, which is not a rule the engine follows, and a microsecond is finer than
either side of the comparison carries. Both moments have to be columns.

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

A `MIN` or `MAX` answers its column's own kind, so the two columns are recorded
and held to the rule `column IN (SELECT column ...)` holds its pair to — a word
column against a number aggregate is refused, which is the comparison MySQL
answers by coercion, with a 1292 warning per row. A `COUNT` answers a whole
number whatever it counts, so it meets a whole number written out and nothing
else. `SUM` and `AVG` are left out: MySQL answers `AVG` as a decimal rounded to
four places where the engine keeps the whole fraction, so a comparison against
one can land on either side of a row.

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

`IFNULL` and `COALESCE` take an aggregate as the thing they default, which is
how a report asks for a total over rows that may not be there —
`IFNULL(SUM(amount), 0)` — or for the highest of nothing —
`COALESCE(MAX(id), 0)`. Measured on MySQL 8.4.11, each answers the shape its
aggregate answers on its own: the NEWDECIMAL of length 33 that `SUM` over an
INT answers, the length 16 and scale 4 of `AVG`, the length 34 and scale 2 of
`SUM` over a `DECIMAL(10,2)`, the length 21 of `COUNT`. What the wrapper adds
is NOT_NULL, and a widening of any whole number to a BIGINT that leaves the
length alone — `IFNULL(MAX(s), 0)` over a `SMALLINT` answers LONGLONG with the
SMALLINT's length 6. That is the same widening `IFNULL` over a plain column
does, so the two forms are one rule. Over no rows at all the answer is the
fallback rather than NULL, which is the whole reason the call is written.

The fallback still has to be a whole number, which is the rule the plain-column
form follows. A call inside the call rather than an aggregate, and `NULLIF`,
which answers the other way round, are refused.

An `ON DUPLICATE KEY UPDATE` value may join the row already there to the one
offered, which is how a counter is stepped: `hits = hits + 1`, or
`hits = hits + VALUES(hits)` to step it by the number offered. A bare column is
the row already there in both MySQL and the engine, and `VALUES(col)` is the
offered one, which the engine calls `excluded.col`.

Since MySQL 8.0.19 the offered row may carry a name instead — `VALUES (...) AS
offered ... hits = t.hits + offered.hits` — which is the spelling that replaces
`VALUES()`. Measured on 8.4.11 over (1, 10, 'a') and (2, 20, 'b'), running the
whole sequence and matched row for row: stepping row 1 leaves 11, a row that is
not there is inserted as it stands, row 2 goes 20 to 27 to 30, and a word taken
from the offered row lands as written.

Once the offered row carries a name, a bare column is 1052 — ambiguous between
the two rows — so only a qualified one names either, and the bare form is
refused here as it is there. A qualifier naming neither row is refused, so is a
name on the offered row with no upsert to use it, and so is renaming what that
row carries, which has not been measured.

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
their table and the real name as the original table. Naming the table an alias
renamed — `SELECT a.* FROM a AS t` — answers 1051 in MySQL and is refused here
too. A qualifier carrying a schema, `db.t.*`, is answered by MySQL but refused
here, being a source name that has to be resolved across databases first.

A grouping key may be qualified or not, and matches a projection either way,
which is what MySQL does whenever the bare name is unambiguous; the engine
answers the ambiguous case itself. Two forms are refused: a grouping key that is
not a whole column, and the `WITH ROLLUP` modifier, which changes what a group
is.
Grouping on a text column carries the collation divergence: MySQL puts `abc` and
`ABC` in one group and this puts them in two.

`DISTINCT` is taken, and `DISTINCTROW` with it, since that is MySQL's own
synonym. It drops repeats among the projected values and leaves the result
metadata exactly as it would be without it. The two engines agree about numbers
and disagree about text, for the collation reason above: `SELECT DISTINCT name`
over `abc` and `ABC` is one row in MySQL and two here, because MySQL's default
collation makes them equal and the engine compares them byte for byte. Unlike a
`WHERE`, this cannot be fixed by asking for `NOCASE`, since the parser does not
know which projected columns are text. `DISTINCT ON` is refused, being no part
of MySQL.

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

Three things are refused. A non-integer column operand, since MySQL's decimal
and float arithmetic carry their own precision and scale rules and those have
not been measured. A nested division, which makes every operator above it
decimal arithmetic for the same reason. And a column operand with no `FROM` to
resolve it against, though `SELECT 1+1` with no table runs.

An integer result that leaves `BIGINT`'s range answers 1690 / 22003, as MySQL
does — measured. The engine turns the same sum into a float rather than failing,
and a float where a column promised an integer is exactly that overflow, so both
protocols answer the error rather than sending a number the column's type does
not describe.

A window, a filter, more than one argument and an
expression argument stay refused; DISTINCT on aggregates other than COUNT remains refused;
`GROUP_CONCAT` with `SEPARATOR`, `ORDER BY`, or `DISTINCT` stays refused.

`REPLACE INTO` is taken, over the same `VALUES` shape an ordinary `INSERT`
takes. MySQL's `REPLACE` deletes the rows a unique key collides with and
inserts, which is what the engine's own `OR REPLACE` does, so the rows it leaves
behind are MySQL's. The affected count is not: measured on 8.4.11, a new row
counts 1, a replaced one counts 2 because it is a delete and an insert, and
`REPLACE INTO r VALUES (2, 30), (1, 40)` over an existing row 1 counts 3. The
engine does not count the delete, so this counts the inserts alone — 1, 1 and 2
for those three statements. Everything an ordinary `INSERT` refuses —
`ON DUPLICATE KEY UPDATE`, `IGNORE` — a `REPLACE` refuses too, and everything it
takes, the `SET` form included, a `REPLACE` takes.

`INSERT ... ON DUPLICATE KEY UPDATE` is taken. It is an upsert, and the engine
has the same one: MySQL's clause fires on a collision with any unique key, and
the engine's `ON CONFLICT DO UPDATE`, written without a conflict target, does
too. `VALUES(col)` names the value the row was offered, which the engine spells
`excluded.col`. The columns the clause does not name are left as they were, in
both.

The affected count differs, as it does for `REPLACE`, and for the same kind of
reason. Measured on 8.4.11 over a table holding (1, 10): inserting (2, 30)
counts 1, updating row 1 to a different value counts 2 — MySQL counts the
attempted insert and the update — and an update that leaves the row identical
counts 0. This counts the row once and the identical update once: 1, 1 and 1.
The engine's upsert rewrites the row whether or not the value moved, so its
changed-row counter sees a write either way.

The clause is refused where it would be dropped rather than answered: on the
`SET` form and the empty-row form, which leave no room for it, and beside
`REPLACE` or `IGNORE`, which already decide what a collision does. It is also
refused on an `AUTO_INCREMENT` table, for the reason `IGNORE` is.

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
one the INSERT writes, which is the table it actually compares against. A SELECT that would need a second rendering pass to learn its
column types — one ordering a bare column, or comparing a `?` — is refused,
because there is no way to ask for that pass from a DML statement.

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

`INSERT IGNORE` into an `AUTO_INCREMENT` table is refused. The allocator
reserves its range before the rows are written, so a row IGNORE skips has
already taken a number, and what MySQL reports as the last insert id for a
statement whose rows were all skipped has not been measured.

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
NULL — a call and a variable differ in both. Every other variable name is left
to the reader that owns it: `@@sql_notes` has its own, the driver's
`SELECT @@max_allowed_packet,@@wait_timeout` has its own, and a name no reader
claims is refused rather than answered with a value this server does not have.

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
`utf8mb4_general_ci`, which is what this frontend runs on. Any other character
set or collation is refused.

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
Every other mode is refused rather than quietly ignored.

`SET time_zone` is taken for `+00:00`, `-00:00`, `UTC` and `SYSTEM`. Nothing
here converts a moment between zones, which is the same as running in UTC, so
any other zone would be a claim this cannot keep. `SET information_schema_stats_expiry`
is taken for any value: it is how long MySQL caches `information_schema`
statistics, and there are none here.

A column may name a `CHARACTER SET` or a `COLLATE`, which a dumped schema
spells out on every text column, so refusing them stopped a `mysqldump` from
being restored. `utf8mb4` is taken, and so are `utf8mb4_general_ci` and
`utf8mb4_0900_ai_ci` — the ones this server already claims, in `SET NAMES` and
in the `SHOW CREATE TABLE` trailer. Naming any other is refused rather than
ignored: `utf8mb4_bin` compares case-sensitively and this does not, so taking it
would answer a different set of rows.

The words are written nowhere, because the engine has no place to keep them, and
that is a divergence in what is echoed back rather than in what the column does.
Measured on 8.4.11, `SHOW CREATE TABLE` repeats the clause even when it names
the table default; here the column comes back without it.

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
there is nothing to hold. `NOWAIT`, `SKIP LOCKED` and `OF <table>` are refused: each asks for
something one lock over one database cannot answer. `LOCK IN SHARE MODE`, MySQL's older
spelling of `FOR SHARE`, is not read by the parser yet.

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

An `AUTO_INCREMENT` table is refused instead. MySQL restarts the counter at 1,
and the durable allocator behind this frontend only ever moves its high water
forward, so a table taken here would go on handing out the keys it left off at
while MySQL starts again — a difference in what a later row is called, which is
not one to make quietly. Resetting the allocator is what this needs, and it is
not a thing the sidecar can do yet.

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
on the database rather than on one session.

A wildcard is refused: it asks for MySQL's twenty-one columns and this answers
three, so a row of a different width would come back. So is a call over one of
these columns, whose shape has not been measured; counting them works, since a
count does not depend on what a column holds. The one written shape this
recognized before still answers, because it carries a `WHERE TABLE_SCHEMA =
DATABASE()` that the checked `SELECT` surface does not read yet — a query
written without that predicate goes the general way.

`information_schema.STATISTICS` is the second such table, and the first this
frontend has ever answered. It reports one row per column of every index of
every table the session may see: the primary key first under the name
`PRIMARY`, then each index once per column it holds, with `NON_UNIQUE`,
`SEQ_IN_INDEX` and the `NULLABLE` of the column indexed — which is what a
migration tool reads to find out what indexes exist. Seventeen of MySQL's
eighteen columns are answered. `CARDINALITY` is the one left out: it is an
estimate of distinct values, and the engine keeps no equivalent, so answering a
made-up number would be worse than answering none. A comparison against one of
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

A `CHECK` constraint has no row in `TABLE_CONSTRAINTS`. The engine keeps it in
the stored DDL rather than in the schema these tables read, and a table
carrying one is already refused by `SHOW CREATE TABLE` for the same reason.

An `information_schema` query names the columns it wants, in the order it wants
them, and is answered that way. The catalog answers three of MySQL's twenty-one
`TABLES` columns — `TABLE_SCHEMA`, `TABLE_NAME`, `TABLE_TYPE` — and seven of its
twenty-two `COLUMNS` ones. Which of those a query names, and in what order, is
up to the query. A column outside the set is refused rather than answered with a
value that would be made up: `TABLE_ROWS` and the rest of the table's statistics
are numbers this server does not keep. The same column named twice is refused
too, where MySQL answers it twice — what a row holds is never wider than the
whole row, which is what the result's size is measured against. `ORDER BY` may
be left off: the rows come back in table-name order, and a table's columns in
declaration order, whether or not the query asks for it.

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

What differs is the index. Measured: InnoDB creates a `KEY` beside the
constraint — `KEY \`fk_b\` (\`b\`)` — and keeps it after the constraint is
dropped, where nothing here creates one, so `SHOW CREATE TABLE` differs by that
line. An `ADD FOREIGN KEY` written without a name is refused, because MySQL
would name it `t_ibfk_N` counting the keys the table already carries, which is
naming this does not do.

`DROP KEY` is MySQL's other spelling for `DROP INDEX` and drops the same key.
The parser library reads only the second, so the words are swapped before it
sees them — on the tokens rather than on the text, so only the word right after
a `DROP` is read as the keyword and a column called `key` is left where it is.

Three shapes are refused. An unnamed key, for the same reason the inline clause
refuses one — though the rule is now measured, `ADD INDEX (a)` three times over
names them `a`, `a_2` and `a_3`, and a key over `(a, b)` after those is `a_4`, so
it is implementing it that is left. A statement mixing index and column
operations, which would have to apply two kinds of change together. And the
name `PRIMARY`, since adding or dropping a primary key is a different operation
than adding or dropping an index.

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
and so does a table that counts its own ids. Anything else is a claim this cannot
keep and is refused rather than quietly dropped — measured, `DEFAULT
CHARSET=latin1`, `COLLATE=utf8mb4_bin`, `ENGINE=MyISAM` and `ROW_FORMAT=DYNAMIC`
are each printed back. `AUTO_INCREMENT=<n>` is refused as well: it is a number the
allocator would have to start from, not a label. The same option written twice is
refused, MySQL taking the last one written.

A table may write its key as a clause of its own — `PRIMARY KEY (id)` after the
columns — as well as on the column that carries it. That is the spelling MySQL's
own `SHOW CREATE TABLE` prints, so it is the one every dumped schema and every
migration built from a dump writes, while this reads a key only where the column
declares it. The words are moved onto the column named, so the one reader answers
both, and what this prints can be handed back to it.

Measured on 8.4.11 and matched: the printed schema is the same whichever way the
key was written, a key over a column written nullable makes that column `NOT
NULL`, a `CONSTRAINT` name on the key is dropped — the key always being named
`PRIMARY` — an `ASC` on the column is dropped, and the column named is matched
without regard to case. A `USING BTREE` and a `DESC` are printed back, so both
stay refused, and so does a key over several columns, which has no one rowid to
stand for it. A key naming a column the table does not have and a table writing
two keys are refused where MySQL answers 1072 and 1068.

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

`START TRANSACTION WITH CONSISTENT SNAPSHOT` is taken, which is what
`mysqldump --single-transaction` opens with — inside the versioned comment
`/*!40100 WITH CONSISTENT SNAPSHOT */`, which the tokenizer expands, so both
spellings arrive the same. It begins a transaction, and one thing about it
differs from MySQL: MySQL takes the read view at the statement, and the engine's
`BEGIN` is deferred, so the view is taken at the first read. Everything read
after that comes from one view, which is the property a dump needs; what a dump
can pick up that MySQL's would not is a row committed in the gap between the
statement and the first read.

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

`SET [SESSION] TRANSACTION ISOLATION LEVEL REPEATABLE READ` is taken, which is
what a connection pool sends when it opens. Measured on 8.4.11,
`REPEATABLE-READ` is MySQL's default and it is the level these sessions run at,
so a client naming it is describing where it already is. The other three levels
are refused rather than accepted and ignored: a client told yes to
`SERIALIZABLE` would go on reasoning about a guarantee it does not have. The
`GLOBAL` scope is refused for the same reason — it changes what other sessions
get. With no scope word MySQL applies the level to the next transaction rather
than the session, which makes no difference to a server that answers only the
level it is already in.

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
`DATE` column is refused, the checked comparison path knowing integers and text
and not yet what a date compares against.

A `TIME` holds a span rather than a moment, and the difference shows in what it
takes: measured on 8.4.11 it runs from `-838:59:59` to `838:59:59`, so it takes
a sign and more than a day. The column reports type 11 with length 10, the width
of the widest span without its sign, decimals 0 and the binary flag, and
`SHOW CREATE TABLE` prints `time`. `CURTIME()` and `CURRENT_TIME` answer the
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
`49:01:01`. A `WHERE` comparison against a `TIME` column is still refused.

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

`RAND()` answers a double between zero and one, NOT NULL, as MySQL's does. A
seeded `RAND(n)` is refused: the engine has no seeded random, so answering one
would answer a different sequence. `UUID()` answers a thirty-six character
identifier — the engine's is a random one where MySQL's is time-based, so the
two differ in kind while both are identifiers. `MD5` answers the same
thirty-two hexadecimal characters MySQL answers.

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
length 9, counting the date alone and dropping any time either carries, and
both its arguments have to hold a date.

`DATE_ADD(column, INTERVAL n unit)` and `DATE_SUB` shift a date. Measured on
8.4.11: an interval of whole days, months or years keeps the column's own kind
— a `DATE` stays a `DATE` of length 10 and a `DATETIME` keeps its time — while
an interval carrying an hour, a minute or a second answers a `DATETIME` of
length 19 either way. Which of the engine's two readers to ask therefore
depends on the column, and the rendering layer does not know column types, so
the stored text says it instead: a `DATE` is exactly the ten characters of
`YYYY-MM-DD`. Weeks and quarters are refused, the engine having no modifier for
either, and a `TIME` column is refused for holding no date to shift.

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

A `SET` does **not** order the same way here: MySQL orders one by its numeric
value, one bit for each member, so `read, write, exec` come back in that order
there where this answers them alphabetically.

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

`JSON_EXTRACT(doc, '$.path')` reads one path out of a document and answers the
JSON value it found, so a string comes back with its quotes;
`JSON_UNQUOTE(JSON_EXTRACT(...))` takes those quotes off; and `JSON_VALID`
answers one or zero. Measured on 8.4.11: the first reports the JSON type at
length 4294967292, the second a LONG_BLOB at the widest length there is, both
with the text collation and the binary flag, and the third a LONGLONG of 21
with the binary collation. The paths taken are the plain member-and-element
ones — `$`, `$.a`, `$[0]`, `$.a[1]` — which MySQL and the engine read the same
way; MySQL's wildcards, `$.*`, `$[*]` and `$**`, are refused rather than read
a different way, and so is a call naming more than one path. MySQL's operator
spellings of the first two, `doc -> '$.a'` and `doc ->> '$.a'`, read the same
and are named after the text they were written with, as MySQL names them.

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
`FROM_UNIXTIME` reads one back. MySQL reads both in the session's time zone,
and this session takes only UTC — nothing here converts a moment between zones,
which is the same as running in UTC — so the two agree. Measured on 8.4.11 with
`time_zone = '+00:00'`: a `DATE` reads as its midnight, a moment before the
epoch counts 0 rather than a negative, where the engine counts backwards, and a
negative count reads no moment at all, where the engine reads one before the
epoch. The count is a LONGLONG of 21 with the binary and numeric flags —
NOT NULL for `UNIX_TIMESTAMP()`, which reads now and so has nothing that could
be null — and the moment a DATETIME of 19 with the binary flag alone.

The count is read out of a `DATE`, a `DATETIME` or a `TIMESTAMP` and the moment
out of a whole number; MySQL reads either by coercing the other, which this has
not measured. `FROM_UNIXTIME(n, format)` is refused, its width being a rule of
its own.

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

The four that change a document take a path naming one member of the top-level
object — `$.a`, not `$.a.b` or `$.a[0]`. That is the range the two agree on.
Measured on 8.4.11 against the engine: MySQL leaves `JSON_SET('{}', '$.x.y',
1)` alone where the engine builds the missing parent, and MySQL appends
`JSON_SET('[1,2]', '$[5]', 9)` where the engine leaves it. A one-step path
cannot reach either disagreement, so the wider paths are refused rather than
answered differently.

Two numbers are stored **more accurately** than MySQL stores them:
`1000000000000000.1` and `1e-30` read back as themselves here, where MySQL
answers `1e15` and `9.999999999999999e-31`. Both are rapidjson's fast path
landing on the double next to the right one.

A `FOREIGN KEY` is taken and **enforced**. The engine has the enforcement and
these connections now run with it on, which is what makes taking the syntax
honest: until now the constraint was refused precisely because a stored one
would never have been checked. Measured on 8.4.11: a child row naming a
parent that is not there answers 1452 and a parent row still named by a child
answers 1451, both SQLSTATE 23000. The engine reports one failure for both
directions, so this answers 1452 either way — the direction a client meets
first. Turning enforcement on took nothing away from a table already stored:
the constraint was refused until now, so no durable table carries one.

`SHOW CREATE TABLE` prints the constraint as MySQL names it, `` `t_ibfk_1` ``,
counted from one in declaration order, with its `ON DELETE` and `ON UPDATE`
where they were written. Two things differ. A named constraint —
`CONSTRAINT fk_parent FOREIGN KEY ...` — is refused, the engine dropping the
name, and printing MySQL's generated one in its place would be a quiet
substitution. And MySQL's InnoDB creates an index on the child column and
prints it — measured, `` KEY `a` (`a`) `` — where this creates none, so the
printed table differs by that one line.

An inline `REFERENCES` on a column is read and written nowhere, which is what
MySQL does with it. Measured on 8.4.11: `parent_id INT REFERENCES p(id)`
stores a child row naming a parent that does not exist, and `SHOW CREATE
TABLE` prints no constraint at all, whatever `ON DELETE` or `ON UPDATE` was
written beside it. The table-level `FOREIGN KEY (a) REFERENCES p(id)` is a
different statement, which MySQL does enforce, and it stays refused.

`GROUP_CONCAT` takes a `SEPARATOR` and a `DISTINCT`, one at a time. MySQL
writes the separator as a clause after the column and the engine as a second
argument, and the default is a comma in both, so the two agree over the same
rows: measured on 8.4.11, `GROUP_CONCAT(name SEPARATOR '-')` answers `x-y-z`
and `GROUP_CONCAT(DISTINCT team)` answers `a,b` where the plain call answers
`a,a,b,a`. The two together are refused, the engine taking `DISTINCT` only
over a single argument and the separator being that second one, and so is an
`ORDER BY` inside the call: MySQL orders the parts it joins and the engine's
`group_concat` has no way to say in what order it joins them.

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
`@@version_comment`, `@@sql_mode`, `@@autocommit`, `@@sql_notes`, `@@max_allowed_packet` and
`@@wait_timeout`, under any scope and under an alias. `@@sql_mode` is not a setting here — the
modes MySQL's own default names are the ones this enforces, and a client asking for another is
refused rather than told it took effect — so the answer is that list, with `ANSI_QUOTES` and
`NO_BACKSLASH_ESCAPES` added when the session was opened with them. MySQL writes the modes in
an order of its own rather than the order they were set in, measured, and so does this.

Their shapes are measured on 8.4.11: a word answers the same `VAR_STRING` of length 87380 with
31 decimals and no flags that `@@version` does; `@@autocommit` and `@@sql_notes` answer a
`LONGLONG` of length 1 carrying the binary and numeric flags; and `@@max_allowed_packet` and
`@@wait_timeout` answer a `LONGLONG` of length 21 carrying those and the unsigned flag. The
two counters answer this server's own values rather than MySQL's defaults, which is what makes
them honest.

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

The declared name an unsigned `DECIMAL` is stored under puts the sign the other
way round, `UNSIGNED DECIMAL(10,2)`, because the engine's declared type takes a
word before its arguments and not after them. MySQL's word order goes back on
where the column is read, so nothing above the reader sees the inversion.

`SHOW CREATE TABLE` prints an unsigned integer the way MySQL does, the sign a
second lower-case word after the type — `tinyint unsigned`, `smallint
unsigned`, `mediumint unsigned`, `int unsigned`, `bigint unsigned`, all
measured on 8.4.11, with `INTEGER UNSIGNED` printing as `int unsigned` the way
plain `INTEGER` prints as `int`. Before this the statement refused any table
carrying one.

`INT UNSIGNED AUTO_INCREMENT PRIMARY KEY` is taken, which is the spelling a
MySQL schema usually gives a surrogate key. The allocator counts in an i64 and
4294967295 fits one, so nothing about the numbering changes. How high it may
count is the column's own type rather than a fixed ceiling: an `INT` stops at
2147483647 and an `INT UNSIGNED` at 4294967295, so an `UPDATE` that moves the
counter to 3000000000 is taken on the second and refused on the first.

`BIGINT UNSIGNED` is taken up to `i64::MAX` and refused above it, which is a
divergence rather than a gap. Its top value, 18446744073709551615, is more than
twice `i64::MAX`, and the engine holds an integer as an `i64`, so the top half
of the range has nowhere to go. What can be stored behaves as MySQL does —
measured on 8.4.11, a LONGLONG of 20 reporting UNSIGNED, printed
`bigint unsigned`, and a negative answering 1264 — and 9223372036854775808
answers 1264 as well, where MySQL stores it. Answering there is the honest
half: rounding a value into an `i64` would put the wrong row behind a key,
which is what the type is usually holding. `BIGINT UNSIGNED AUTO_INCREMENT` is
still refused, because the allocator takes the `INT` spellings and neither
`BIGINT` is one of them.

`BOOLEAN` and `BOOL` are taken as what MySQL makes them: a `TINYINT` carrying
the display width one. `SHOW CREATE TABLE` and `SHOW COLUMNS` print
`tinyint(1)` for either spelling, and a result column reports the TINYINT type
with length 1, where a plain `TINYINT` reports 4 — measured. The value is a
`TINYINT`'s and is held to a `TINYINT`'s range, so 999 is refused.

`DATETIME` holds whole seconds in MySQL's own text form, and takes the wide
input surface MySQL takes: measured on 8.4.11, `'2026-9-6 1:2:3'`,
`'2026-09-06'`, `'20260906010203'` and `'2026-09-06T01:02:03'` are all read and
stored as `YYYY-MM-DD HH:MM:SS`, and `'...01:02:03.5'` rounds up to the next
second, carrying into the next day and the next year where it has to.

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
and `SHOW COLUMNS` print `datetime`, and a result column reports type 12 with
length 19 and the binary flag, because a temporal column carries no collation —
measured. A fractional-second precision, `DATETIME(3)`, is refused.

`DECIMAL(p,s)` is taken without the exactness the type exists for. The engine
has no exact decimal, so the value is held as the same binary64 a `DOUBLE` uses:
three `0.1` rows sum to exactly `0.30` in MySQL and to `0.30000000000000004`
here, measured. One difference follows: MySQL rounds to the declared scale on
the way in, half away from zero — `12.345` into a `DECIMAL(10,2)` stores `12.35`
and `12.335` stores `12.34`, measured — and this stores what it was given.

The rendering does match. MySQL writes a decimal at the scale the column
declared, and so does this: a `DECIMAL(10,2)` holding 1.5 reads back as `1.50`
over both protocols, an `AVG` answers `2.0000` at its scale of four, and `3/2`
answers `1.5000`.

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

Every column type this frontend answers crosses the binary protocol as well as
the text one. `CHAR`, `DECIMAL`, `DATETIME` and `TIMESTAMP` each arrived with a
text answer and no binary one, so a prepared `SELECT` of any of them failed
where the same statement over the text protocol worked. MySQL sends a `CHAR` and
a `DECIMAL` as length-encoded text and a temporal value as fields — a length
byte and then that many bytes, nothing at all for a zero value, the date alone
when the time is midnight, and the date and time otherwise — and that is what
these send now. The eleven-byte microsecond form never arises, because this
server keeps whole seconds.

`FLOAT` is taken, with the binary32 rounding done where a client can see it
rather than where the value is stored. MySQL keeps a `FLOAT` in binary32 and the
engine has only binary64, so a value stored here keeps more of itself than MySQL
would have. Both protocols round it back on the way out: the text protocol
renders the binary32 nearest the stored value, so `0.1` reads back as `0.1`
rather than as the binary64 nearest a binary32 `0.1`, and the binary protocol
sends the four bytes a `FLOAT` column's four bytes are. What still differs is
what a later computation sees — a `SUM` over the column adds binary64 values
where MySQL adds binary32 ones.

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
| `CREATE TABLE` | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend tests`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [key clause](conformance/cases/p0/create-table-key-clause.json), [table options](conformance/cases/p0/create-table-options.json) oracle cases, [P0 manifest](conformance/Makefile), [`mysql_async` Unix E2E](runtime/tests/unix_e2e.rs) | Conservative marked-DDL subset only, including ordinary signed `INT`/`INTEGER PRIMARY KEY` and identity-backed v2 `AUTO_INCREMENT` DDL. The key may be written on the column or as a `PRIMARY KEY (col)` clause of its own, the spelling MySQL prints and every dumped schema carries; the clause is read by moving the words onto the column named, and a clause over several columns, one carrying `USING BTREE` or `DESC`, one naming a column the table does not have, and a table writing two keys all stay refused. Ordinary primary keys lower to a regular SQLite `INT NOT NULL PRIMARY KEY` without a rowid alias; the durable v1 marker retains source integer spelling and the `ENGINE = InnoDB` label. A table may carry the trailer MySQL prints after every table — `ENGINE=InnoDB`, a `CHARSET`/`CHARACTER SET` of `utf8mb4` and a `COLLATE` of `utf8mb4_0900_ai_ci`, in any of MySQL's spellings — which names the table this makes anyway and is taken and left out, so a printed schema can be handed straight back; another character set, another collation, another engine, `ROW_FORMAT`, `AUTO_INCREMENT=<n>` and a repeated option are all refused. An ordinary-PK table is rewritten through the same renderer every other table's is, which is what lets an `ALTER TABLE` run against one; a counted table goes through that renderer told which column it counts on, so its rewrite keeps the column's declared spelling and the marker that makes it counted; a rewrite drops the source integer spelling and the `ENGINE` label, neither of which a client can see. Auto-increment tables remain creatable, reopenable, and replayable through the identity-backed embedded frontend, with execute-only literal `INSERT ... VALUES` generation in registry-selected embedded sessions. The authorized command adapter executes the checked DDL subset through text `COM_QUERY` after database selection and authorization; the external-driver E2E covers `CREATE TABLE`. Qualified names and wider forms remain rejected. `TEMPORARY` is taken for an ordinary table and gated only for the AUTO_INCREMENT form. Non-binary character contexts and prepared DDL remain closed. `AS SELECT` is taken over a checked one-table `SELECT` whose projected items are plain columns, aliased or not, or a lone `*`: the new columns are read out of the source table's stored DDL, keeping type, `NOT NULL` and `DEFAULT`, dropping keys, and replacing a dropped `AUTO_INCREMENT` with a zero default, and the `CREATE` and its `INSERT` run inside one transaction. It reports the rows copied. Expression columns, string defaults, declared columns beside the `SELECT`, `IF NOT EXISTS` and `TEMPORARY` are rejected there. |
| `ALTER TABLE` | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/alter-counted-table.json), [P0 manifest](conformance/Makefile), [architecture limits](../docs/mysql-compatibility-mode.md) | Existing checked text DDL dispatch accepts one supported operation at a time. View- and trigger-dependent rewrites retain the documented restrictions. An ordinary-PK table takes the column operations, and a `MODIFY`/`CHANGE` of the key column itself is refused. A table that counts its own ids takes them too and goes on counting, its rewrite writing the counted column the way it was declared; `DROP COLUMN`, `RENAME COLUMN` and `MODIFY COLUMN` of that column are refused, where MySQL drops it and leaves an ordinary table. Prepared DDL remains unsupported. Several operations in one statement are split into one MySQL statement each and run inside one transaction, so the statement applies whole or not at all. `ADD INDEX`, `ADD KEY`, `ADD UNIQUE INDEX` and `DROP INDEX` become one `CREATE INDEX` or `DROP INDEX` each, under the same all-or-nothing transaction, and answer 1061 for a name the table already carries and 1091 for one it does not. `DROP KEY` is taken as MySQL's other spelling of `DROP INDEX`. An unnamed key, the name `PRIMARY`, and a statement mixing index and column operations are refused. `MODIFY COLUMN` and `CHANGE COLUMN` restate one column whole — an attribute the statement does not restate is dropped, as MySQL drops it — and become the engine's `ALTER COLUMN`; `FIRST` and `AFTER` are refused because the engine cannot move a column, and an unknown column answers 1054. |
| `DROP TABLE` | partial | partial | experimental | planned | partial | [`checked parser`](parser/drop_table.rs), [`frontend session`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Accepts exactly one unqualified non-internal table name with optional `IF EXISTS` and one trailing semicolon. Qualified or multiple names and extra clauses are rejected; prepared DDL remains unsupported. Base-table removal, missing table/view handling, `sql_notes` warnings, and the preceding-transaction commit boundary are covered. |
| `TRUNCATE TABLE` | partial | partial | experimental | planned | partial | [`checked parser`](parser/truncate_table.rs), [`frontend session`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Accepts exactly one unqualified non-internal table name, with the `TABLE` keyword optional and one trailing semicolon. Qualified or multiple names, extra clauses, and comments are rejected; prepared DDL remains unsupported. An unfiltered `DELETE` does the emptying between a commit on each side, so the statement cannot be rolled back and the write before it is committed, and it reports 0 affected rows. An unknown name and a view both answer 1146. An `AUTO_INCREMENT` table is refused, because MySQL restarts the counter at 1 and the durable allocator only moves its high water forward. |
| Indexes | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Existing checked text DDL dispatch accepts conservative ordinary and unique index creation. Prepared DDL remains unsupported. |
| Views | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Existing checked text DDL dispatch accepts simple one-table view creation. `DROP VIEW` is committed in `8a756dca1`; prepared DDL remains unsupported. |
| Triggers | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | One `AFTER INSERT FOR EACH ROW` form with a single `INSERT ... VALUES` body. |
| MySQL-owned file marker | partial | experimental | n/a | n/a | partial | [`core dialect`](../core/dialect/mod.rs), [`fresh-process tests`](../core/multiprocess_tests.rs) | New MySQL files use and enforce format-v2 marker `0x54520224` (`lower_case_table_names=1`). PostgreSQL v1 remains valid; legacy MySQL v1 and unknown/mismatched policy bits fail closed. Offline legacy migration and policy `0` are not implemented. |
| Logical databases | partial | experimental | experimental | planned | partial | [`database registry`](frontend/database_registry.rs), [`DatabaseCatalog`](frontend/database_catalog.rs), [`Unix capability backend`](frontend/filesystem_backend.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`persistent account store`](server/src/persistent_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs), [`Unix runtime`](runtime/src/main.rs), [`core capability`](../core/database.rs), [D007 plan](../docs/mysql-compatibility-plan.md) | The strict admin parser accepts only plain `CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES`; trusted embedded sessions and the authorized `COM_QUERY` adapter execute them through the same typed catalog operations. The registry owns main, WAL, two inode-bound metadata sidecars, and one durable AUTO_INCREMENT allocator sidecar per database. Creation initializes and syncs the allocator identity header before sidecar-first publication; acquire, recovery, and drop verify it through retained descriptors. Real-backend failure and replacement-race tests keep recovery fail closed. Registry-selected embedded sessions retain the allocator and execute the narrow generated-ID INSERT slice. The public Unix catalog shares one root across independent sessions without exposing paths or descriptors. Each session owns at most one selected connection; successful switches release the old lease and failed switches preserve it. Names are canonicalized and authorized before catalog access; denied or unavailable policy returns 1045 without revealing existence, while only authorized missing names return 1049. Create/drop authorization receives the target name, use shares the connect action, list is global and all-or-nothing, and selected-database queries are reauthorized on every command. The same-UID Unix worker supplies the persistent policy and catalog to a real protocol stream; the standalone runtime owns the `RuntimeUnixServer` accept loop and worker reaper. The CI cross-UID external-driver E2E covers `USE`, ordinary writes, prepared writes, and reads through this path. Preopened `VACUUM`, physical restore without re-key/regenerated sidecars, and shared-WAL/MVCC authority remain unsupported; the current protocol surface is the documented conservative DML subset. |
| `SHOW TABLES` | partial | partial | experimental | planned | partial | [`checked parser`](parser/lib.rs), [`table/view listing`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Accepts plain `SHOW TABLES` and confirmed `SHOW FULL TABLES`, each with an optional single semicolon. A database must already be selected, and the selected database must pass `DatabaseAction::Query` authorization before catalog access. Returns user-visible base tables and views in name order, excluding SQLite/Turso internal tables; when database-wide `Query` is denied, the result is filtered to tables granted through the table `Select` action. The catalog scan uses a 4,097-row sentinel and the protocol result is bounded to 4,096 rows, per-value size, and total retained result memory. A `LIKE 'pattern'` filters the list and puts the pattern in the column name, so `SHOW TABLES LIKE 'alpha%'` answers a column called `Tables_in_probe (alpha%)`; measured on MySQL 8.4.11 the pattern matches a table name by case, unlike every other `SHOW ... LIKE`, which matches whatever the case. `FROM`, `IN`, and `WHERE` remain unsupported. |
| Narrow `information_schema.TABLES` query | partial | n/a | experimental | n/a | partial | [`checked parser`](parser/lib.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-tables.json), [P0 manifest](conformance/Makefile) | Accepts only the checked `TABLE_SCHEMA`/`TABLE_NAME`/`TABLE_TYPE` projection with `TABLE_SCHEMA = DATABASE()` and name ordering. Selected-database `Query` authorization runs before catalog access; when database-wide `Query` is denied, the result is filtered through table `Select` grants. The result is bounded and lists user tables and views. The checked MySQL oracle case/golden is a reference contract and is listed in the P0 manifest, but it is not a Turso execution gate. Other `information_schema` providers and cross-database coverage remain incomplete. |
| `information_schema.STATISTICS` query | partial | n/a | experimental | n/a | partial | [`catalog tables`](frontend/catalog_tables.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-statistics.json), [P0 manifest](conformance/Makefile) | A table the engine scans, so the ordinary `SELECT` path answers it: any projection of the seventeen columns this reports, any `WHERE` over them and any `ORDER BY`. One row per column of every index of every table the session may see, the primary key first as `PRIMARY`. `CARDINALITY` is the one MySQL column left out — it is an estimate of distinct values the engine keeps no equivalent of. Every reported shape is pinned to the MySQL 8.4.11 golden. A wildcard is refused, because it asks for eighteen columns and this answers seventeen; so is a call over one of these columns, whose shape has not been measured. What a session may see is filtered exactly as for `TABLES`. |
| `information_schema.KEY_COLUMN_USAGE` query | partial | n/a | experimental | n/a | partial | [`catalog tables`](frontend/catalog_tables.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-key-column-usage.json), [P0 manifest](conformance/Makefile) | A table the engine scans, answered by the ordinary `SELECT` path. One row per column of every primary key, unique key and foreign key of every table the session may see; a plain index constrains nothing and has no row. All twelve of MySQL's columns are answered, every shape pinned to the 8.4.11 golden. A foreign key reports its parent table and column and its position in the key it references; a primary or unique key leaves those NULL. A key written without a name is reported as `t_ibfk_N`, matching `SHOW CREATE TABLE`. A wildcard is refused, the same as for every one of these tables. What a session may see is filtered exactly as for `TABLES`. |
| `information_schema.TABLE_CONSTRAINTS` / `REFERENTIAL_CONSTRAINTS` queries | partial | n/a | experimental | n/a | partial | [`catalog tables`](frontend/catalog_tables.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-table-constraints.json), [P0 manifest](conformance/Makefile) | Tables the engine scans, answered by the ordinary `SELECT` path. `TABLE_CONSTRAINTS` reports one row per primary key, unique key and foreign key of every table the session may see; `REFERENTIAL_CONSTRAINTS` reports one row per foreign key with the key it references, its `MATCH_OPTION`, and the `UPDATE_RULE` and `DELETE_RULE` it was written with — measured, a key written with no rule reads back as `NO ACTION` and `RESTRICT` reads back as written. Both answer all of MySQL's columns, every shape pinned to the 8.4.11 golden. A `CHECK` constraint has no row: it lives in stored DDL rather than in the schema these read. A wildcard is refused, the same as for every one of these tables. What a session may see is filtered exactly as for `TABLES`. |
| `information_schema.COLUMNS` contract | partial | n/a | experimental | n/a | experimental | [`checked parser`](parser/lib.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-columns.json), [P0 manifest](conformance/Makefile) | The exact `COLUMN_NAME`/`ORDINAL_POSITION`/`COLUMN_DEFAULT`/`IS_NULLABLE`/`COLUMN_TYPE`/`COLUMN_KEY`/`EXTRA` projection, `TABLE_SCHEMA = DATABASE()` filter, validated selected-database table/view target, and ordinal ordering are accepted by the checked parser and provider. The former fixed `records` target is now arbitrary per query; the pinned `records` case/golden remains the reference contract. The narrow unnamed explicit column-`NULL` form is durable and its frontend metadata is tested, including the restored `MEDIUMINT NULL` fixture. Named or conflicting nullable attributes remain rejected. Selected-database `Query` authorization runs before lookup, with the table `Select` fallback for the requested target; missing or denied targets return an empty result and internal tables remain hidden. Golden metadata is pinned, and scan, row, value, packet-payload, and retained-memory bounds are checked before staging output. The pre-release `MySqlInformationSchemaColumnsQuery` now stores a private target and is no longer `Copy`; callers construct it through the parser and read `table()`. Other providers and cross-database coverage remain incomplete. |
| `LIMIT ?` / `LIMIT ? OFFSET ?` / `LIMIT ?, ?` | partial | partial | n/a | n/a | partial | [`limit renderer`](parser/translate.rs), [`row count validator`](frontend/session.rs) | A row count binds like any other parameter. Each spelling is rendered as it was written, so a `?` keeps the ordinal the client bound it at — the comma spelling writes the offset first. What is bound is held to a whole number at or above zero, because the engine reads a negative row count as no limit at all where MySQL refuses one. A `LIMIT` in an `UPDATE` or `DELETE` still takes a written number only. |
| `UPDATE ... SET` assigning arithmetic over the row — `SET n = n + 1` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/update-arithmetic-assignment.json), [P0 manifest](conformance/Makefile) | A column is read in an assignment, and `+`, `-` and `*` over one. Division is refused: measured, `b / 2` over 101 answers 50.5 in MySQL and 50 in the engine. Counting past a column's range is refused and the row keeps what it had, where MySQL answers 1690. A value naming a column the same `SET` has already assigned is refused, because MySQL reads the assigned value there and the engine reads the row as it was. Every answer is pinned to the 8.4.11 golden. |
| `CURDATE()` / `NOW()` / `CURTIME()` as a value to write | partial | partial | n/a | n/a | partial | [`value renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/insert-now-value.json), [P0 manifest](conformance/Makefile) | Written by `INSERT ... VALUES`, `INSERT ... SET`, `ON DUPLICATE KEY UPDATE` and `UPDATE ... SET`. The column puts the value into the form it holds, so a moment into a `DATE` keeps the day and a day into a `DATETIME` becomes midnight, both measured. A moment into a word is the moment written out and one too wide is refused with 1406. Two differences: MySQL raises 1292 for the time dropped going into a `DATE` and this drops it quietly, and a moment into a number is refused here where MySQL runs it together into a fourteen-digit one. Every answer is pinned to the 8.4.11 golden. |
| `COUNT(*) OVER ()` — a window over the whole result | partial | partial | n/a | n/a | partial | [`window reader`](parser/static_select_metadata.rs), [oracle case](conformance/cases/p0/select-window-over-the-whole-set.json), [P0 manifest](conformance/Makefile) | An aggregate over a window naming neither a partition nor an order answers the whole set's value beside every row, in the shape its windowed form already reports. A ranking over the same window keeps its refusal, having no order to rank by. |
| `WHERE <column> = '...' COLLATE ...` | partial | partial | n/a | n/a | partial | [`comparison renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-comparison-collate.json), [P0 manifest](conformance/Makefile) | Written on the column or on the value, either way. `utf8mb4_bin` compares the bytes and the case-ignoring ones compare the way naming none compares. A collation over a `LIKE`, over a membership test, over a number, over a bound value, or from another character set is refused. |
| `ORDER BY <column> COLLATE ...` | partial | partial | n/a | n/a | partial | [`order renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-order-by-collate.json), [P0 manifest](conformance/Makefile) | `utf8mb4_bin` orders by bytes, which is the engine's own order, so the ordering asks for no collation. `utf8mb4_0900_ai_ci` and `utf8mb4_general_ci` order the way naming none does. A collation from another character set is 1253 there and refused here, as is one over something that is not a column. |
| `BIN`, `OCT`, `FIELD`, `ELT` | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`dialect`](frontend/dialect.rs), [oracle case](conformance/cases/p0/select-radix-and-place.json), [P0 manifest](conformance/Makefile) | The engine has none of the four, so the dialect answers them. `BIN` and `OCT` write a whole number out — a negative one by its bits — and refuse a word, which MySQL reads as the number it names. `FIELD` answers 0 for a word that is not among the choices and for one that is nothing at all. `ELT` answers nothing past the last choice. The choices are written out, being what the answer's width comes from. |
| `PI`, `DEGREES`, `RADIANS` | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`call renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-math-readings.json), [P0 manifest](conformance/Makefile) | `PI()` answers 3.141593 — six places, not the whole number — reporting NOT NULL. Turning an angle round is one multiplication and the two work it out alike. `SIN`, `COS`, `TAN`, `ASIN`, `ACOS`, `ATAN`, `EXP`, `LN`, `LOG`, `LOG2` and `LOG10` are refused: two of them already differ in the last place. |
| `REGEXP` / `RLIKE` | partial | partial | n/a | n/a | partial | [`predicate renderers`](parser/translate.rs), [`dialect`](frontend/dialect.rs), [oracle case](conformance/cases/p0/select-regexp.json), [P0 manifest](conformance/Makefile) | Answered by the dialect, matching without regard to case and with regard to accents, which is what the collation does here. Anchors, character classes, repeats, choices, any-character and the negated form are pinned to the golden. A pattern looking ahead or naming a group again, a pattern that does not close, a bound pattern and a match over a number are refused. |
| Arithmetic touching a `DOUBLE` — `d + 1`, `SUM(d) * 2` | partial | partial | n/a | n/a | partial | [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-double-arithmetic.json), [P0 manifest](conformance/Makefile) | A DOUBLE of length 23 with 31 decimals, whichever side the float was on, whichever operator it was, and whatever the other side was. A float swallows the precision rules rather than taking part in them, and so does an aggregate over one. |
| Arithmetic over an aggregate or a decimal — `SUM(amount) * 2`, `amount + 1` | partial | partial | n/a | n/a | partial | [`arithmetic classifier`](parser/static_select_metadata.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-aggregate-arithmetic.json), [P0 manifest](conformance/Makefile) | An aggregate stands where a column stands. Three measured rules cover adding, multiplying and dividing, over whole numbers and decimals alike. Whether the answer is a decimal is not whether it carries places: `SUM(n) + 1` is one and `COUNT(*) + 1` is not. `GROUP_CONCAT`, a deviation and a windowed aggregate are refused. |
| A column beside an aggregate with no `GROUP BY` | partial | partial | n/a | n/a | partial | [`aggregated projection`](parser/translate.rs), [oracle case](conformance/cases/p0/select-aggregated-projection.json), [P0 manifest](conformance/Makefile) | Refused, where MySQL answers 1140 — a column anywhere in the projection, not only one standing on its own. A literal crosses. A window and a subquery do not aggregate the statement, and a `GROUP BY` gives every column a group. |
| `DATE_FORMAT(NOW(), ...)` / `STR_TO_DATE('...', ...)` — a moment that is not a column | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`call renderer`](parser/translate.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-moment-argument.json), [P0 manifest](conformance/Makefile) | A clock reading and a moment written out as a word stand where a column stands. Both report exactly what the column form reports: the shape comes from the format. `NOW`, `CURRENT_TIMESTAMP`, `CURDATE` and `CURRENT_DATE` are the readings taken; a `STR_TO_DATE` reads text, so it takes a word and not a reading. |
| `TIMESTAMPDIFF(<unit>, a, b)` | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`call renderer`](parser/translate.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-timestampdiff.json), [P0 manifest](conformance/Makefile) | Whole units from the first moment to the second, over the units of fixed length — SECOND, MINUTE, HOUR, DAY and WEEK. A whole number of length 21, where `DATEDIFF` reports 9. MONTH, QUARTER, YEAR and MICROSECOND are refused. |
| `USE` / `FORCE` / `IGNORE INDEX` | partial | partial | n/a | n/a | partial | [`table source renderer`](parser/translate.rs), [`hint validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-index-hint.json), [P0 manifest](conformance/Makefile) | Dropped: a hint says which key to plan with and nothing about which rows come back. The keys it names are checked against the table, because one naming a key the table has not got is 1176 in MySQL. Both spellings, a `FOR` scope, several keys at once, an alias and either side of a join are covered. A hint on an `UPDATE` or `DELETE` target is still refused. |
| `QUARTER`, `WEEKDAY`, `DAYOFWEEK`, `DAYOFYEAR`, `DAYOFMONTH`, `LAST_DAY`, `EXTRACT` | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`call renderer`](parser/translate.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-calendar-readings.json), [P0 manifest](conformance/Makefile) | The engine has none of these by name, so each is counted off what it does have. Every value and every reported shape is pinned to the 8.4.11 golden, `EXTRACT(YEAR FROM ...)` included, which reports a whole number of length 5 where `YEAR` reports a YEAR of length 4. |
| `LIKE CONCAT('%', ?, '%')` — a pattern written in pieces | partial | partial | n/a | n/a | partial | [`LIKE renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-like-concat-pattern.json), [P0 manifest](conformance/Makefile) | The pieces spell one pattern, and written ones are joined into it. A bound piece stays a piece and the join is left to the engine. A piece naming a column, a second bound piece, and a backslash in any piece are refused. |
| `WHERE n = (SELECT MAX(n) FROM t)` — a comparison against a subquery | partial | partial | n/a | n/a | partial | [`comparison renderer`](parser/translate.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-scalar-subquery-comparison.json), [P0 manifest](conformance/Makefile) | A `MIN` or `MAX` over one implicit group, held to the same kind rule `IN (SELECT ...)` holds its columns to, and a `COUNT` against a whole number written out. A plain-column projection is refused: MySQL answers 1242 over many rows where the engine takes the first. `SUM` and `AVG` are refused for their rounding. |
| `WHERE 1 = 1 AND ...` — a comparison naming no column | partial | partial | n/a | n/a | partial | [`predicate renderers`](parser/translate.rs), [oracle case](conformance/cases/p0/select-constant-predicate.json), [P0 manifest](conformance/Makefile) | Two whole numbers compared, and a bare whole number as the predicate, which is the opening a statement built up in pieces uses. It holds in a `SELECT`, an `UPDATE` and a `DELETE`. A word against a word and a number against a word stay refused, MySQL reading those without regard to case and by coercion. |
| `IFNULL(SUM(n), 0)` / `COALESCE(MAX(n), 0)` — an aggregate with a fallback | partial | partial | n/a | n/a | partial | [`call classifier`](parser/static_select_metadata.rs), [`result metadata`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-defaulted-aggregate.json), [P0 manifest](conformance/Makefile) | The shape the aggregate answers on its own, plus NOT_NULL, with any whole number widened to a BIGINT and the length left alone. Over no rows the answer is the fallback rather than NULL. The fallback has to be a whole number, the rule the plain-column form already follows. |
| `ON DUPLICATE KEY UPDATE hits = hits + VALUES(hits)` — a counter stepped | partial | partial | n/a | n/a | partial | [`upsert renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/insert-upsert-counter.json), [P0 manifest](conformance/Makefile) | A bare column is the row already there and `VALUES(col)` the one offered, which the engine calls `excluded.col`; arithmetic joins the two. A name put on the offered row — MySQL 8.0.19's replacement for `VALUES()` — names the same thing, and once it is there a bare column is 1052 and refused. |
| `UPDATE ... SET <column> = <call>` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/update-set-call.json), [P0 manifest](conformance/Makefile) | A call or a `CASE` writes a value worked out from the row, rendered the way a projection renders it. A value reading a column the same `SET` has already written is refused: MySQL takes the assignments left to right and the engine reads the row as it stood. |
| `UPDATE ... SET` dividing a column — `SET ratio = n / 2` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/update-set-division.json), [P0 manifest](conformance/Makefile) | Decimal division, rounded to the column's own scale on the way in. The divisor has to be a written number that is not zero, and a fraction written into a whole-number column is refused. |
| A `LIKE` pattern's escape — `LIKE 'a\_b'`, `ESCAPE 'x'` | yes | yes | n/a | n/a | yes | [`LIKE renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-like-escape.json), [P0 manifest](conformance/Makefile) | The clause says what MySQL would have taken: a backslash by default, the named character when one is named, and nothing under `NO_BACKSLASH_ESCAPES`. An escape of more than one character is refused. |
| `RENAME TABLE old TO new` | partial | partial | n/a | n/a | partial | [`rename reader`](parser/alter_table_indexes.rs), [oracle case](conformance/cases/p0/rename-table.json), [P0 manifest](conformance/Makefile) | Written into the `ALTER TABLE` shape. Several tables at once are refused, and so are the two error shapes, where MySQL answers 1050 and 1146. |
| `DROP INDEX name ON table` | yes | yes | n/a | n/a | yes | [`index reader`](parser/alter_table_indexes.rs), [oracle case](conformance/cases/p0/drop-index-on-table.json), [P0 manifest](conformance/Makefile) | Written into the `ALTER TABLE` shape the reader already answers. An index that is not there is 1091, and the spelling with no table is refused. |
| `information_schema.COLUMNS` naming its database — `TABLE_SCHEMA = 'db'` | yes | yes | n/a | n/a | yes | [`catalogue reader`](parser/information_schema.rs), [oracle case](conformance/cases/p0/information-schema-columns-named.json), [P0 manifest](conformance/Makefile) | Taken beside the `DATABASE()` spelling. The name is read as it was written, and any database but the selected one answers no rows. |
| A `WHERE` testing a column on its own — `WHERE active` | partial | partial | n/a | n/a | partial | [`predicate renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-bare-flag.json), [P0 manifest](conformance/Makefile) | Read as a comparison against zero, as MySQL reads it. A column of words is refused, and so is one tested in an `UPDATE` or a `DELETE`. |
| `IFNULL` / `COALESCE` with a written word — `IFNULL(email, 'none')` | partial | partial | n/a | n/a | partial | [`defaulted classifier`](parser/static_select_metadata.rs), [oracle case](conformance/cases/p0/select-defaulted-word.json), [P0 manifest](conformance/Makefile) | The column's own width whatever the word's is, NOT NULL, and `VAR_STRING` even over a `CHAR`. A `TEXT` column and a word over a column of numbers are refused. |
| A join `ON` naming a value — `ON t.id = u.team_id AND t.name = 'red'` | yes | yes | n/a | n/a | yes | [`join predicate renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-join-on-value.json), [P0 manifest](conformance/Makefile) | Goes through the reader a `WHERE` comparison goes through, so the value is held to the column's own type. Column against column stays equality; an `ON` in an `UPDATE` or `DELETE` takes columns alone. |
| `ORDER BY` over a call — `ORDER BY LOWER(name)` | yes | yes | n/a | n/a | yes | [`ORDER BY renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-order-by-call.json), [P0 manifest](conformance/Makefile) | Any call whose shape is already known, collated the way a text column is. A random number is refused. |
| A derived table — `FROM (SELECT ...) x` | partial | partial | n/a | n/a | partial | [`derived table renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-derived-table.json), [P0 manifest](conformance/Makefile) | The body reads one table and projects its columns, which the alias then stands for. Its result columns carry the table's own shapes. A wildcard, an expression, a join inside the body, a `LATERAL` one and a missing alias are refused, the last being MySQL's own 1248. |
| `DATE_ADD` / `DATE_SUB`, month ends and the week and quarter units | yes | yes | n/a | n/a | yes | [`shift arithmetic`](parser/shift_moment.rs), [oracle case](conformance/cases/p0/select-month-end-shift.json), [P0 manifest](conformance/Makefile) | The shift is worked out by the frontend rather than by the engine, whose month arithmetic overflows a day the target month has not got. A quarter is three months and a week seven days. A count worked out from a row is refused. |
| `CONCAT` over a number — `CONCAT(name, id)` | partial | partial | n/a | n/a | partial | [`spelled characters`](../mysql/server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/select-concat-numbers.json), [P0 manifest](conformance/Makefile) | A number is laid end to end with the words, spelling as many characters as its type does. Integers, `BOOLEAN`, `YEAR` and the temporal types are taken; a `DECIMAL`, a `FLOAT` and a `DOUBLE` are refused, MySQL spelling those its own way. |
| `HAVING` naming a projection alias — `HAVING c > 1` | yes | yes | n/a | n/a | yes | [`alias resolver`](parser/translate.rs), [oracle case](conformance/cases/p0/select-having-alias.json), [P0 manifest](conformance/Makefile) | A name is the projection's alias before the table's column, measured, and is resolved to what it stands for before the clause is read. Covers an aggregate alias, the grouped column's alias, two at once, no `GROUP BY`, and an aliased column filtering rows. |
| A `CASE` or `IF` whose branches are numbers | partial | partial | n/a | n/a | partial | [`branch classifier`](parser/static_select_metadata.rs), [oracle case](conformance/cases/p0/select-numeric-branches.json), [P0 manifest](conformance/Makefile) | Taken in a projection and in a `SET`. The answer is a `LONGLONG` as wide as its widest branch plus one for the sign, NOT NULL only when every branch is and there is an `ELSE`. A branch carrying a scale, and a word branch beside a number branch, are refused. |
| An integer column's display width — `INT(11)`, `TINYINT(1)` | yes | yes | n/a | n/a | yes | [`column renderer`](parser/lib.rs), [oracle case](conformance/cases/p0/create-table-display-width.json), [P0 manifest](conformance/Makefile) | Taken and dropped, which is what MySQL 8.4 does with one; the counted column takes one too. `TINYINT(1)` is kept and is the same stored type as `BOOLEAN`, reporting a length of 1 where `TINYINT` reports 4. MySQL's warning 1681 is not raised. |
| `INSERT` writing an `AUTO_INCREMENT` column its own ids | partial | partial | n/a | n/a | partial | [`written ids`](../mysql/frontend/session.rs), [oracle case](conformance/cases/p0/insert-written-auto-increment.json), [P0 manifest](conformance/Makefile) | The counter is raised past the highest id written, so a later counted row never repeats one. Measured and matched: rows out of order, an id below the counter, a negative id, the reported id being the last row's, and `LAST_INSERT_ID()` staying as it stood. A written 0 or NULL is refused, and so is a statement writing some rows and counting others. |
| `INSERT ... VALUES` with `DEFAULT` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/insert-default-value.json), [P0 manifest](conformance/Makefile) | `DEFAULT` and `DEFAULT(col)` naming that same column ask for the column's own default, and are rendered by leaving the column out — measured, MySQL answers the same value, the same NULL and the same 1364 for both. An `AUTO_INCREMENT` column counts on. `DEFAULT` in one row and a value in another is refused, so is every column of a counted table, so is `DEFAULT` beside `ON DUPLICATE KEY UPDATE`, and so is `SET n = DEFAULT` on an `UPDATE`. |
| `UPDATE ... SET <column> = (SELECT ...)` | partial | partial | n/a | n/a | partial | [`assignment renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/update-set-subquery.json), [P0 manifest](conformance/Makefile) | A value taken out of another table. The subquery has to answer exactly one row, which an aggregate over one implicit group does and a plain column does not — MySQL answers 1242 for that one. Reading the table being changed is refused, MySQL's 1093. The column written and the column read are held to the same kind, so a `COUNT(*)` and a word into a column of numbers are both turned away. |
| `UPDATE` / `DELETE` naming rows through a subquery | partial | partial | n/a | n/a | partial | [`DML predicate renderer`](parser/translate.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/dml-subquery-predicate.json), [P0 manifest](conformance/Makefile) | `WHERE id IN (SELECT ...)`, `NOT IN` and `EXISTS`, each answering the rows MySQL answers — `NOT IN` over a list holding NULL matches nothing at all. The subquery's table is authorized as a table the statement reads, and its column is held to the same kind rule a `SELECT` holds it to. A subquery reading the table being changed is refused, where MySQL answers 1093. |
| `SELECT a.*` — a wildcard over one source | partial | partial | n/a | n/a | partial | [`projection renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-qualified-wildcard.json), [P0 manifest](conformance/Makefile) | The source's columns in declaration order, each naming its own table. It mixes with a plain column and with a second wildcard, and an alias renames the source for it. A qualifier carrying a schema — `db.t.*` — is refused. |
| `HAVING` with no `GROUP BY` over an unaggregated statement — `SELECT id FROM t HAVING id > 1` | partial | partial | n/a | n/a | partial | [`row-filter reader`](parser/translate.rs), [oracle case](conformance/cases/p0/select-having-without-group-by.json), [P0 manifest](conformance/Makefile) | MySQL filters rows, not groups, so the test is written into the `WHERE`. It may name only a column the projection carries; an unprojected one is 1054 there and stays refused here. |
| `ORDER BY <column> IS NULL` | partial | partial | n/a | n/a | partial | [`order renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-order-by-nulls.json), [P0 manifest](conformance/Makefile) | The idiom for sending the rows holding nothing last. Both answer the test as 0 or 1 and sort by that, so the orders agree — measured with the flag written either way round and in either direction. The test has to be over a column. |
| `(a, b) IN ((1, 'x'), ...)` | partial | partial | n/a | n/a | partial | [`row list renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-row-in.json), [P0 manifest](conformance/Makefile) | Written out as the question it means — each row's columns joined by `AND`, the rows joined by `OR` — so every column is held to its own type, a word is read under the collation, and a row holding NULL is left out of the `NOT IN` as well as the `IN`. A member that is not a row, or one of a different width, is refused. Every answer is pinned to the 8.4.11 golden. |
| A call on the left of a comparison — `WHERE LOWER(email) = 'a'` | partial | partial | n/a | n/a | partial | [`comparison renderer`](parser/translate.rs), [`answer of a call`](parser/static_select_metadata.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-call-comparison.json), [P0 manifest](conformance/Makefile) | The call says what it answers and the value it meets is held to that. A word is compared without regard to case, the way MySQL compares one after the call answers; a number meets a number; a day and a moment are held to the form one is stored in. The calls answering a real number are left out, and a `?` meets none of them. Every answer is pinned to the 8.4.11 golden. |
| `CAST(col AS CHAR / SIGNED / DATE / DATETIME)` | partial | partial | n/a | n/a | partial | [`cast classifier`](parser/static_select_metadata.rs), [`cast renderer`](parser/translate.rs), [oracle case](conformance/cases/p0/select-cast.json), [P0 manifest](conformance/Makefile) | The four targets the engine answers exactly what MySQL answers. `CHAR` writes a whole-number or temporal column out, as wide as the column's display width in utf8mb4 bytes. `SIGNED` rounds away from zero before it casts, because MySQL rounds and the engine's cast cuts. `DATE` and `DATETIME` read the day and the moment out. `UNSIGNED`, `DECIMAL`, `CHAR(n)`, a real or `DECIMAL` column written out, and a word read as a number or a day are each refused for a measured reason. `CONVERT(col, <type>)` is read as the same thing; `CONVERT(col USING <charset>)` and the T-SQL spellings are refused. |
| `DATE_ADD` / `DATE_SUB` over a reading of the moment | partial | partial | n/a | n/a | partial | [`shift renderer`](parser/translate.rs), [`call metadata`](parser/static_select_metadata.rs), [oracle case](conformance/cases/p0/select-shifted-moment.json), [P0 manifest](conformance/Makefile) | Read in a projection, as a value to write, and on the right of a comparison. Measured: shifting `NOW()` answers a nullable `DATETIME` of 19 whatever the interval, and shifting `CURDATE()` a nullable `DATE` of 10 for whole days, months or years and a `DATETIME` otherwise. A shifted day meets a `DATE` column and a shifted moment a `DATETIME` or `TIMESTAMP`. `CURTIME()` is not shifted: a span is not a moment. Which rows each comparison finds is pinned to the 8.4.11 golden. |
| `WHERE` comparison against `CURDATE()` / `NOW()` / `CURTIME()` | partial | partial | n/a | n/a | partial | [`comparison reader`](parser/lib.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-now-comparison.json), [P0 manifest](conformance/Makefile) | Each is rendered as the engine call answering the same value in the same form — `date('now')`, `datetime('now')`, `time('now')` — and meets the column whose form it answers in: a day meets a `DATE`, a moment a `DATETIME` or `TIMESTAMP`, and a time of day a `TIME`, for sameness only. Both spellings of each, with and without parentheses, are read. Any other call on the right of a comparison is still refused. Every answer is pinned to the 8.4.11 golden. |
| `WHERE` comparison against a number written with a fraction — `money > 9.99` | partial | partial | n/a | n/a | partial | [`comparison reader`](parser/translate.rs), [`comparison validator`](frontend/session.rs), [oracle case](conformance/cases/p0/select-decimal-literal-comparison.json), [P0 manifest](conformance/Makefile) | Read as the number it names and carried into the rendered SQL as it was written, so the engine reads the same number. It meets any column that holds a number, whole or not; a text column is refused, the mirror of a string against an integer column. A run of digits too long for an `i64` keeps its refusal rather than becoming the nearest number it names. A `HAVING` still takes only a whole number, being counted against a count. Every answer is pinned to the 8.4.11 golden. |
| `LOCK TABLES` / `UNLOCK TABLES` | partial | partial | n/a | n/a | partial | [`lock parser`](parser/lock_tables.rs), [`write lock`](frontend/session.rs) | The lock is really held, until `UNLOCK TABLES`: it is the engine's write lock, held by the write transaction the statement opens, and a session that writes while it is held waits and answers 1205. One lock over the whole database rather than one for each table, so `READ` and `WRITE` take the same one and the names are read and let go. The statements between commit together at the unlock, so `START TRANSACTION`, `COMMIT` and `ROLLBACK` are refused while it is held rather than dropping the lock. `READ LOCAL`, `LOW_PRIORITY WRITE` and `LOCK INSTANCE FOR BACKUP` are refused. |
| `SELECT ... FOR UPDATE` / `FOR SHARE` | partial | partial | n/a | n/a | partial | [`lock reader`](parser/translate.rs), [`write lock`](frontend/session.rs) | The lock is really held: the statement takes the engine's write lock by writing no row, and another session that writes while it is held waits for it and answers 1205 once the wait runs out, which starts at MySQL's fifty seconds and is changed by `SET SESSION innodb_lock_wait_timeout`. It is one lock over the whole database rather than one for each row, so it is stronger than MySQL's. Outside a transaction none is taken, which is what MySQL's amounts to there. `NOWAIT`, `SKIP LOCKED` and `OF <table>` are refused. |
| `WHERE` comparison against a `DATE` / `DATETIME` / `TIMESTAMP` / `TIME` / `YEAR` / `DECIMAL` / `DOUBLE` / `FLOAT` / `ENUM` / `SET` column | partial | partial | n/a | n/a | partial | [`comparison validator`](frontend/session.rs), [`temporal values`](parser/temporal_value.rs), [oracle case](conformance/cases/p0/select-temporal-comparison.json), [P0 manifest](conformance/Makefile) | These columns hold the canonical form MySQL stores, so a comparison against a value already written that way answers the rows MySQL answers, whatever each row was written as. A day and a moment read in order read in time order, so every operator works; a `TIME` runs past a day and carries a sign, so only `=`, `!=`, `<=>` and `IN` are answered for one. A `YEAR` and a real are compared as numbers. A value written any other way is refused rather than rewritten — measured, `d = '2024-1-1'`, `dt = '2024-01-01'` and `y = 24` each find rows in MySQL that comparing the stored form would not — and so is a `?`, which is not put into that form when it binds. An `ENUM` or `SET` member spelled the way it was declared is compared for sameness; a member spelled another way, a number naming a member's position, and any ordering comparison are refused, because MySQL reads each of those by a rule the stored spelling does not meet. Every answer above is pinned to the 8.4.11 golden. |
| Signed `TINYINT` / `SMALLINT` / `MEDIUMINT` / `INT` / `BIGINT` assignment | partial | partial | rejected | planned | partial | [`numeric parser`](parser/lib.rs), [`assignment validator`](frontend/dialect.rs), [numeric oracle case](conformance/cases/p0/numeric-coercion.json), [MEDIUMINT oracle case](conformance/cases/p0/numeric-mediumint.json) | Strict signed ranges are checked before storage for marked columns: `TINYINT` −128..127, `SMALLINT` −32,768..32,767, `MEDIUMINT` −8,388,608..8,388,607, `INT` −2,147,483,648..2,147,483,647, and `BIGINT` `i64::MIN..i64::MAX`. The checked `INSERT`/`UPDATE` path covers parameters, multi-row rollback, triggers, TEMP/attached schemas, reopen, and `VACUUM`; durable DDL and metadata retain the width. String/real coercion, expressions, other widths, permissive warnings, casts, arithmetic, ordering, and protocol errors remain rejected or unimplemented. |
| `SHOW COLUMNS` / `DESCRIBE` / `EXPLAIN table` | partial | partial | experimental | planned | partial | [`checked parser`](parser/lib.rs), [`frontend metadata`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [pinned case](conformance/cases/p0/show-columns.json) | Only plain `SHOW COLUMNS FROM table`, `DESCRIBE table`, `DESC table`, and `EXPLAIN table` — measured on MySQL 8.4.11, `EXPLAIN t` prints exactly what `DESCRIBE t` prints — with one canonical unqualified table or one canonical marked view with a direct projection from one base table, plus an optional single semicolon, are accepted. The selected database is required; database-level `Query` authorization runs before metadata lookup, with an exact table `Select` grant as the narrow fallback. Table metadata comes from verified normalized MySQL DDL and typed defaults, including `PRI` and `auto_increment` for the checked primary auto-increment form. Direct-view metadata verifies persisted view rootpage, SQL, and base-column provenance; it preserves projected type and nullable metadata while clearing table-only `Key`, `Default`, and `Extra`. View chains, projection/source aliases, expressions, joins, qualified or system sources, and duplicate output names are rejected. Frontend metadata preserves declared `INT` versus `INTEGER` spelling, while the wire `Type` column canonicalizes both to `int`. Unknown extras fail closed. The pinned case/golden covers this metadata; scan, row, value, packet, and retained-memory bounds apply. A `LIKE` pattern names the columns to report, and `DESCRIBE t <name>` reads a name after the table the same way. `FULL` adds `Collation`, `Privileges` and `Comment`, the first from the column type and the last always empty, and answers NULL for `Privileges`, which MySQL fills from the user grants this server does not keep per column. Qualification, comments, `WHERE`, `DESCRIBE TABLE t`, a pattern after `EXPLAIN`, and multiple statements remain rejected; `information_schema` is not a substitute and remains incomplete. |
| Table-specific `SELECT` grants (persistence and narrow enforcement) | n/a | n/a | partial | partial | partial | [`account store`](server/src/account_store.rs), [`snapshot format`](server/src/account_store_format.rs), [`authorization API`](server/src/authorization.rs), [`persistent store`](server/src/persistent_account_store.rs), [`runtime store`](server/src/runtime_account_store.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`offline provisioner`](offline-provisioner/src/main.rs) | Canonical database/table names, the bounded `select` permission, duplicate/order rules, legacy decoding, durable restart, runtime reload/revocation, and `--table-grant DATABASE.TABLE:select` provisioning are covered in the policy backend/CLI. When database-wide `Query` is denied, the adapter falls back only for parser-confirmed canonical unqualified one-table text or prepared `SELECT`, checks the table `Select` action, and reauthorizes prepared execution against its origin database. Joins, multiple sources, qualified sources, internal catalogs, and unsupported query shapes do not use the fallback. SQL `GRANT`/`REVOKE` and catalog filtering beyond the selected-database narrow path remain open; the final recorded privileged Linux gate passed the table-grant selector. |
| Unsigned integers and `DECIMAL` | rejected | rejected | rejected | planned | planned | [D004 plan](../docs/mysql-compatibility-plan.md) | Fail closed until exact representation, rounding, overflow, ordering, metadata, and diagnostics pass differential gates. |
| `utf8mb4_0900_ai_ci` comparisons | planned | planned | planned | planned | planned | [collation oracle case](conformance/cases/p0/collation-utf8mb4-0900-ai-ci.json), [D005 plan](../docs/mysql-compatibility-plan.md) | The implementation must be an immutable built-in provider over frozen UCA 9.0/CLDR 30 data with identical compare/sort-key/hash semantics and a persisted data version. ICU4X 2.2 uses newer CLDR/ICU data and is not an exact substitute. The reproducible data-generation and license/notices path is still pending, so the collation remains rejected. |
| `AUTO_INCREMENT` / `LAST_INSERT_ID()` | partial | partial | partial | partial | experimental | [`checked parser`](parser/lib.rs), [`schema envelope`](frontend/schema_sql.rs), [`durable range primitive`](../core/storage/auto_increment.rs), [sequential](conformance/cases/p0/auto-increment.json), [parallel](conformance/cases/p0/auto-increment-parallel.json), [restart](conformance/cases/p0/auto-increment-restart.json), [key clause](conformance/cases/p0/create-counted-key-clause.json) oracle cases | The checked v2 form accepts exactly one signed `INT`/`INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY` — the key written on the column or as a `PRIMARY KEY (col)` clause of its own, the spelling a dumped schema carries — emits a non-`sqlite_sequence` rowid alias, and is creatable, reopenable, and replayable through the identity-backed embedded frontend. Registry-selected embedded sessions reserve one durable contiguous range at execute time for unqualified INSERTs with an explicit non-ID column list and direct literal VALUES rows. Prepared execution additionally accepts bare `?` values in that same omitted-ID `VALUES` shape: preparation does not reserve, and execution rechecks identity and triggers before reserving, injecting, repreparing, binding, and writing. Rollback and failed execution do not reclaim a durable range; the first generated ID is recorded only after a successful write and remains connection-local across failure and rollback, including across `USE` database switches. The checked `SELECT LAST_INSERT_ID()` path reads that live state through embedded and current protocol SELECT paths. Narrow text and prepared protocol INSERT paths return affected rows and the first generated ID in their OK packets. A marked table takes an `ALTER TABLE` that leaves its counted column alone. Named or numbered markers, expressions, explicit allocator columns, qualified names, `TEMPORARY`, wider INSERT forms, explicit exhaustion handling, and direct connections without an allocator capability remain gated. |
| Checked one-table `UPDATE` | partial | partial | experimental | partial | experimental | [`checked parser`](parser/lib.rs), [`frontend affected rows`](frontend/session.rs), [`core changed-row counter`](../core/connection.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | One unqualified table with no alias, joins, `FROM`, optimizer hints, `RETURNING`, or conflict clause. `ORDER BY` and `LIMIT` are supported via a rowid subquery over integer columns; bare `LIMIT` without `ORDER BY` and non-integer ordering are rejected. Assignment values and predicates use the existing conservative DML forms. Text and prepared protocol execution return bounded OK results. The default affected-row count is rows whose stored key or record changed. `CLIENT_FOUND_ROWS` reports predicate-matched rows instead. Core updates this separate success-only counter for both WAL and MVCC execution, without changing SQLite `changes()`. Multi-table and wider expression forms remain rejected. |
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
